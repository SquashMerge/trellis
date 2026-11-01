"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, readFileSync, writeFileSync, rmSync} = require("node:fs");
const {spawnSync} = require("node:child_process");
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
// A two-file module: `low.rs` and `low/` form one `low` module chip.
sources.set("src/low/extra.rs", "pub fn extra(value: i32) -> i32 { if value > 1 { value } else { 1 } }\n");
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
paths = ["src/low.rs", "src/low/**", "src/leaf_2?.rs", "src/leaf_3?.rs"]
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

async function published(config, callback, extra = new Map(), base = sources) {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of [...base, ...extra]) {
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
    const pin = indexed.json().status.revision;
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
      if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method(), body:request.postDataJSON()});
    });
    await H.connect(page, server, paths.token);
    await page.locator("#view-architecture").click();
    await callback({page, paths, pin, server, requests});
    assert.deepEqual(errors, [], "real local assets and repository text obey the unchanged CSP");
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "browsing does not index");
    for (const row of requests.filter(row => row.url.pathname.endsWith("/architecture"))) {
      assert.equal(row.method, "GET"); assert.equal(row.url.searchParams.get("workbench"), "true");
      assert.ok(["production", "all"].includes(row.url.searchParams.get("role")), "browser explicitly selects its metric population");
      assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
      assert.ok(row.url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`));
    }
    for (const row of requests.filter(row => row.url.pathname.includes("/architecture/rules"))) {
      assert.ok(row.url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`), "rules requests remain checkout scoped");
      const selectedPin = row.method === "GET" ? Object.fromEntries(row.url.searchParams) : row.body;
      assert.equal(selectedPin.indexGeneration, pin.indexGeneration);
      assert.equal(String(selectedPin.indexRevision), String(pin.indexRevision));
      if (row.method === "POST") {
        assert.equal(typeof row.body.baseHash, "string");
        assert.ok(Array.isArray(row.body.operations));
      }
    }
    // Library candidate source is catalog-bound, not a workspace pin read.
    for (const row of requests.filter(row => row.url.pathname.endsWith("/source") && !row.url.pathname.endsWith("/dependencies/source"))) {
      assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
    }
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
async function architecture(selected, aggregate = "max", role = "production") {
  const query = new URLSearchParams({aggregate, role, workbench:"true", indexGeneration:selected.pin.indexGeneration, indexRevision:selected.pin.indexRevision});
  for (;;) {
    const answer = await H.api(selected.server.base, selected.paths.token, "GET", H.scoped(selected.server.rootKey, `architecture?${query}`));
    if (answer.data?.payload?.state !== "building") return answer;
  }
}
const identityKey = identity => JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
const siteTotal = edges => edges.reduce((sum, edge) => sum + edge.count, 0);
const summaryKey = edge => JSON.stringify([edge.sourceLayer, edge.destinationLayer, edge.role, edge.status, edge.excepted, edge.exceptionReason]);
async function ready(page) {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
}
// Inspector and review details load on demand (#448); wait until every list has its page.
async function settled(page) {
  await page.waitForFunction(() => ![...document.querySelectorAll(".architecture-detail-list")].some(list => list.textContent.includes("Loading details…")));
}
// Re-renders replace the SVG; mark the old one so a wait sees the new one.
async function rerendered(page, action) {
  await page.evaluate(() => { const svg = document.querySelector("#architecture-diagram svg"); if (svg) svg.__old = true; });
  await action();
  await page.waitForFunction(() => { const svg = document.querySelector("#architecture-diagram svg"); return svg && !svg.__old; });
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
}
// Summary-first display (#391): open every band, footer and directory to reach file chips.
async function expandAll(page) {
  const toggle = page.locator("#architecture-hidden-toggle");
  if (await toggle.isVisible() && await toggle.getAttribute("aria-expanded") === "false") await rerendered(page, () => toggle.click());
  for (let round = 0; round < 60; round++) {
    const next = page.locator('#architecture-diagram [role=button][aria-expanded="false"]').first();
    if (!await next.count()) return;
    await next.focus();
    await rerendered(page, () => page.keyboard.press("Enter"));
  }
  assert.fail("Architecture display did not settle while expanding");
}
const measured = dto => dto.modules.filter(module => module.declarationCount > 0);
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
  assert.equal(dto.modules.length, 42);
  assert.equal(new Set(dto.modules.map(module => identityKey(module.identity))).size, dto.modules.length);
  const cli = await H.run(H.binary, ["references", "--workspace", paths.workspace, "--pin", `${pin.indexGeneration}:${pin.indexRevision}`, "--limit", "5"], {env:paths.env});
  const references = cli.json().payload;
  // #448: the default view carries counts, not per-site detail; counts equal the CLI at the same pin.
  for (const field of ["production", "tests", "unresolved", "modules"]) assert.equal(dto.references[field], undefined, field);
  for (const field of ["instances", "testOnly", "unassigned"]) assert.equal(dto.rules[field], undefined, field);
  assert.deepEqual(dto.references.counts, references.counts); assert.deepEqual(dto.references.limitations, references.limitations);
  const checked = rulesCheck(selected);
  assert.deepEqual(dto.rules.counts, checked.counts); assert.deepEqual(dto.rules.testCounts, checked.testCounts);
  assert.equal(dto.rules.unassignedCount, checked.unassigned.length);
  // Paged details at the same pin and rules hash reproduce the CLI rows.
  const detailRows = async (kind, limit) => {
    const rows = [];
    for (let offset = 0; offset !== null;) {
      const query = new URLSearchParams({kind, indexGeneration:pin.indexGeneration, indexRevision:pin.indexRevision, rulesHash:dto.rulesFile.contentHash, offset, limit});
      const page = await H.api(selected.server.base, paths.token, "GET", H.scoped(selected.server.rootKey, `architecture/details?${query}`));
      assert.equal(page.status, 200, JSON.stringify(page.data));
      rows.push(...page.data.payload.items); offset = page.data.payload.nextOffset;
    }
    return rows;
  };
  dto.rules.instances = (await detailRows("instances", 3)).filter(row => row.role === "production").map(row => row.instance);
  dto.rules.testOnly = (await detailRows("instances", 3)).filter(row => row.role === "test").map(row => row.instance);
  assert.deepEqual(dto.rules.instances, checked.instances); assert.deepEqual(dto.rules.testOnly, checked.testOnly);
  assert.deepEqual((await detailRows("unresolved", 1)), references.unresolved);
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
  assert.equal(selected.requests.filter(row => row.url.pathname.endsWith("/architecture/details")).length, 0, "drawing reads only the summary");
  await expandAll(page);
  await page.locator("#section-files").click();
  await page.waitForFunction(() => document.body.classList.contains("files-section"));
  assert.equal(await page.locator("#architecture-panel").isVisible(), false);
  await page.locator("#section-diagrams").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section"));
  await ready(page);
  assert.equal(await page.locator("#view-architecture").getAttribute("aria-selected"), "true");
  assert.equal(await page.locator("#diagram-metrics").isVisible(), false, "Architecture hides the independently owned file-tree metric controls after Files round-trip");
  assert.equal(await page.locator("#architecture-diagram [data-trellis-module-id]").count(), measured(dto).length, "returning from Files restores the remembered expanded Architecture, not an empty stale tab");
  assert.ok(measured(dto).length < dto.modules.length);
  assert.ok((await page.locator("#architecture-diagram [role=button]").evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")))).some(label => /files? · metrics unavailable\nno production callables/.test(label)), "files without measured callables are counted, not drawn as Unavailable chips");
  assert.match(await page.locator("#architecture-panel").textContent(), /structural references.*syntax evidence/is);
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), false);
  assert.match(await page.locator("#architecture-panel").textContent(), /production callables/i);
  assert.ok(selected.requests.filter(row => row.url.pathname.endsWith("/architecture")).every(row => row.url.searchParams.get("role") === "production"));
  assert.match(await page.locator("#architecture-panel").textContent(), /Unassigned|unknown/i);
  assert.ok((await page.locator("#architecture-diagram [role=button]").evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")))).some(label => label.includes(hostileLayer)), "clipped labels retain their complete accessible text");
  assert.equal(await page.locator("#architecture-diagram img, #architecture-diagram script, #architecture-diagram foreignObject").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.architectureExecuted), undefined);
  assert.equal(await page.getByRole("button", {name:"Open in Treemap", exact:true}).count(), 0, "Treemap navigation is only offered for a selected module");

  await page.locator("#architecture-notices > summary").click();
  await page.getByRole("button", {name:"Inspect unresolved sites", exact:true}).click(); await settled(page);
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Unresolved structural references");
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /external::unknown/);
  await page.getByRole("button", {name:"Inspect same-layer and unassigned references", exact:true}).click(); await settled(page);
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /src\/unassigned\.rs/);
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /src\/top\.rs.*1 reference sites/is);
  const module = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/});
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-module-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-module-id")).sort()), measured(dto).map(row => identityKey(row.identity)).sort(), "every measured native identity maps once, never by label or position");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.filter(row => row.role === "production").map(summaryKey).sort(), "summary identity includes direction, role, policy and exception");
  await module.click();
  await page.waitForFunction(() => document.querySelector("#architecture-inspector-title")?.textContent === "src/low.rs"); await settled(page);
  const detailRequests = selected.requests.filter(row => row.url.pathname.endsWith("/architecture/details"));
  assert.ok(detailRequests.some(row => row.url.searchParams.get("kind") === "pairs" && row.url.searchParams.get("path") === "src/low.rs"), "the module inspector reads its pairs on demand");
  for (const row of detailRequests) {
    assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration); assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
    assert.equal(row.url.searchParams.get("rulesHash"), dto.rulesFile.contentHash);
  }
  assert.equal(await page.getByRole("button", {name:"Open in Treemap", exact:true}).count(), 1);
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
  assert.equal(await page.locator("#architecture-relative").isChecked(), false);
  assert.match(await page.locator("#architecture-legend").textContent(), /Fixed per-callable bands.*≤5.*>5–15.*>15–30.*>30/);
  const metricReads = selected.requests.filter(row => row.url.pathname.endsWith("/architecture")).length;
  await rerendered(page, async () => { await H.reveal(page, "#architecture-relative"); return page.locator("#architecture-relative").check(); });
  assert.match(await page.locator("#architecture-legend").textContent(), /Relative to visible set/);
  assert.equal(selected.requests.filter(row => row.url.pathname.endsWith("/architecture")).length, metricReads);
  await rerendered(page, async () => { await H.reveal(page, "#architecture-relative"); return page.locator("#architecture-relative").uncheck(); });
  const heat = await module.locator("rect").first().getAttribute("fill"), legend = await page.locator("#architecture-legend").textContent();
  await H.reveal(page, "#architecture-tests"); await page.locator("#architecture-tests").check();
  await page.waitForFunction(count => [...document.querySelectorAll('#architecture-diagram [role=button]')].filter(node => node.getAttribute("aria-label")?.includes("test sites")).length === count, dto.edges.filter(edge => edge.role === "test").length);
  assert.deepEqual(await page.locator('#architecture-diagram [role=button][aria-label*="production sites"]').evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")).sort()), beforeProduction);
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.map(summaryKey).sort(), "test toggle preserves complete exact summary mapping");
  assert.equal(await module.locator("rect").first().getAttribute("fill"), heat);
  assert.equal(await page.locator("#architecture-legend").textContent(), legend, "hiding/showing tests does not reclassify metric population");
  const only = dto.modules.find(row => row.identity.path === "src/only.rs");
  assert.equal(only.declarationCount, 0);
  assert.equal(only.metrics.cognitive, null, "a test-only file has unavailable production heat, never zero");
  const onlyModule = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/only\.rs(?:\s|$)/});
  assert.equal(await onlyModule.count(), 0, "an unmeasured file is counted in its band, not drawn");
  await page.locator("#architecture-unassigned").click();
  await page.locator("#architecture-review-list").getByRole("button", {name:"src/only.rs", exact:true}).click();
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /No production callables/);
  await page.locator("#architecture-review-close").click();
  await module.click();
  await capture(page, "architecture-production-desktop");
  await page.setViewportSize({width:390, height:844});
  if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
  await H.reveal(page, "#architecture-metric-tests");
  assert.equal(await page.locator("#architecture-metric-tests").isVisible(), true, "Options stay reachable at 390px");
  await page.keyboard.press("Escape");
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
  await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").check();
  await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: max, all indexed callables"));
  await ready(page);
  await expandAll(page);
  assert.equal(await page.locator("#architecture-tests").isChecked(), true, "metric inclusion does not alter the independent reference overlay");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.map(summaryKey).sort());
  assert.equal(await module.locator("rect").first().getAttribute("fill"), heat, "0 and 3 remain in the same fixed cognitive band");
  assert.match(await page.locator("#architecture-legend").textContent(), /all indexed callables/);
  assert.doesNotMatch(await onlyModule.textContent(), /Unavailable/, "test-only module becomes measured only when tests are included");
  await module.click();
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /4 indexed callables.*all indexed callables/is);
  await capture(page, "architecture-all-desktop");
  await page.setViewportSize({width:390, height:844});
  if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
  await H.reveal(page, "#architecture-metric-tests");
  assert.equal(await page.locator("#architecture-metric-tests").isVisible(), true, "Options stay reachable at 390px");
  await page.keyboard.press("Escape");
  await capture(page, "architecture-all-mobile");
  await page.locator("#inspector-toggle").click();
  await capture(page, "architecture-all-mobile-inspector");
  await page.setViewportSize({width:1440, height:1000});
  const allHeat = await module.locator("rect").first().getAttribute("fill"), allLegend = await page.locator("#architecture-legend").textContent();
  await H.reveal(page, "#architecture-tests"); await page.locator("#architecture-tests").uncheck();
  await page.waitForFunction(() => ![...document.querySelectorAll('#architecture-diagram [role=button]')].some(node => node.getAttribute("aria-label")?.includes("test sites")));
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), true);
  assert.equal(await module.locator("rect").first().getAttribute("fill"), allHeat);
  assert.equal(await page.locator("#architecture-legend").textContent(), allLegend);
  await H.reveal(page, "#architecture-tests"); await page.locator("#architecture-tests").check();
  await ready(page);
  await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").uncheck();
  await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: max, production callables"));
  await ready(page);
  assert.equal(await page.locator("#architecture-tests").isChecked(), true);
  assert.equal(await module.locator("rect").first().getAttribute("fill"), heat);
  const testEdge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 test sites · upward\s+deviation(?: · .*)?$/});
  await testEdge.focus(); await page.keyboard.press("Enter");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "low → top");
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /2 exact test structural reference sites.*hypothetical policy only/is);
  await H.reveal(page, "#architecture-tests"); await page.locator("#architecture-tests").uncheck();
  await page.waitForFunction(() => ![...document.querySelectorAll('#architecture-diagram [role=button]')].some(node => node.getAttribute("aria-label")?.includes("test sites")));
  const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation(?: · .*)?$/});
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
  assert.equal(await page.getByRole("button", {name:"Open in Treemap", exact:true}).count(), 0, "An edge/source selection is not a module identity");
  await page.locator("#architecture-color").selectOption("layer");
  await page.waitForFunction(() => document.querySelector("#architecture-legend")?.textContent.includes("neutral chips"));
  await ready(page);
  assert.equal(await page.locator('#architecture-diagram [data-trellis-module-id] rect').evaluateAll(nodes => new Set(nodes.map(node => node.getAttribute("fill"))).size), 1, "Layer mode uses neutral module chips");
  assert.equal((await page.locator("#architecture-diagram [data-trellis-module-id]").allTextContents()).some(text => text.includes("Unavailable")), false, "Layer membership is not a missing numeric metric");
  await page.locator("#architecture-color").selectOption("maintainabilityIndex");
  await page.waitForFunction(() => document.querySelector("#architecture-legend")?.textContent.includes("higher is better; heat inverted"));
  await ready(page);
  assert.match(await page.locator("#architecture-legend").textContent(), /≥40.*≥20–<40.*≥10–<20.*<10/);
  await page.locator("#architecture-aggregate").selectOption("sum");
  await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: sum")); await ready(page);
  assert.match(await page.locator("#architecture-legend").textContent(), /relative totals; no callable risk bands/);
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-module-id] rect").evaluateAll(nodes => [...new Set(nodes.map(node => node.getAttribute("fill")))]), ["#14151D"]);
  await page.locator("#architecture-aggregate").selectOption("mean");
  await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: mean"));
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

test("Architecture shows the daemon's building state with progress, then the diagram", {timeout:240000}, async () => {
  await published(rules, async selected => {
    const page = selected.page;
    await ready(page);
    let held = 2;
    await page.route(url => new URL(url).pathname.endsWith("/architecture"), async route => {
      if (held-- <= 0) return route.continue();
      await route.fulfill({status:202, contentType:"application/json", body:JSON.stringify({catchingUp:false, payload:{
        state:"building", ...selected.pin, retryAfterMs:400, progress:{phase:"reading", completed:300, total:604}}})});
    });
    await page.locator("#architecture-aggregate").selectOption("sum");
    const status = page.locator("#architecture-state");
    await status.filter({hasText:"Building Architecture for this revision: reading indexed files 300 of 604"}).waitFor();
    assert.equal(await page.locator("#architecture-diagram").getAttribute("aria-busy"), "true");
    assert.equal(await page.locator("#error").isVisible(), false);
    await capture(page, "architecture-building");
    await ready(page);
    assert.equal(await status.textContent(), "");
    assert.equal(held, -1, "the page asked again until it had the result");
  });
});

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
    await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").check();
    await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: max, all indexed callables"));
    await ready(page);
    await expandAll(page);
    await page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/}).click();
    assert.match(await page.locator("#architecture-inspector-content").textContent(), /4 indexed callables/);
    await page.route(pattern, respond);
    // The production read of this revision is cached on the page (#527); drop it so the gated read is real.
    await page.evaluate(() => clearRevisionReads());
    await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").uncheck();
    await seen;
    assert.equal(await page.locator("#architecture-diagram svg").count(), 0, "changing role removes old heat before the blocked read");
    assert.equal(await page.locator("#architecture-inspector-content").textContent(), "");
    await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").check();
    await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("Metrics: max, all indexed callables"));
    await ready(page);
    const allCanvas = await page.locator("#architecture-diagram").textContent();
    release();
    await page.waitForFunction(() => window.__lateArchitectureDelivered === true);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.match(await page.locator("#architecture-summary").textContent(), /all indexed callables/);
    assert.equal(await page.locator("#architecture-metric-tests").isChecked(), true);
    assert.equal(await page.locator("#architecture-diagram").textContent(), allCanvas, "old delivered response cannot replace newer heat");
  } finally {
    release(); await page.unroute(pattern, respond);
    await page.evaluate(() => { window.fetch = window.__architectureRoleFetch; delete window.__architectureRoleFetch; delete window.__lateArchitectureDelivered; });
  }
}));



const displayStore = selected => selected.page.evaluate(key => JSON.parse(localStorage.getItem(key)), `trellis.architecture.display.v1:${selected.server.rootKey}`);
const bandStates = page => page.locator("#architecture-diagram [data-trellis-layer-id]").evaluateAll(nodes => nodes.map(node => [node.getAttribute("data-trellis-layer-id"), node.getAttribute("aria-expanded")]).sort());
const moduleIds = page => page.locator("#architecture-diagram [data-trellis-module-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-module-id")).sort());
const focused = (page, selector) => page.locator(selector).evaluate(node => document.activeElement === node);

// `trellis rules check` exits nonzero on production deviations; its JSON still reports every instance.
function rulesCheck(selected) {
  const {pin, paths} = selected;
  const result = spawnSync(H.binary, ["rules", "check", "--format", "json", "--workspace", paths.workspace, "--pin", `${pin.indexGeneration}:${pin.indexRevision}`],
    {env:paths.env, encoding:"utf8", maxBuffer:64 * 1024 * 1024, timeout:120000});
  assert.ok(result.stdout, result.stderr);
  return JSON.parse(result.stdout).payload;
}
// Expected header chips per layer from the CLI's own $layers instances (production, outgoing).
function expectedChips(check, layers) {
  const byName = new Map(layers.map(layer => [layer.name, layer])), totals = new Map();
  for (const row of check.instances) {
    if (row.rule !== "$layers" || !row.sourceLayer || !row.destinationLayer || row.sourceLayer === row.destinationLayer) continue;
    const source = byName.get(row.sourceLayer), destination = byName.get(row.destinationLayer), ordered = !source.shared && !destination.shared;
    const key = row.excepted ? "excepted" : row.status === "deviates" ? (ordered && destination.order < source.order ? "↑" : "⇄") :
      row.status !== "conforms" ? "unknown" : destination.shared ? "→shared" : ordered && destination.order > source.order ? "↓" : "allowed";
    const counts = totals.get(row.sourceLayer) || new Map();
    counts.set(key, (counts.get(key) || 0) + row.referenceCount); totals.set(row.sourceLayer, counts);
  }
  const order = ["↑", "⇄", "↓", "→shared", "allowed", "excepted", "unknown"];
  return new Map([...totals].map(([layer, counts]) => [layer, order.filter(key => counts.get(key)).map(key => `${key}${/^[↑⇄↓]$/.test(key) ? "" : " "}${counts.get(key)}`)]));
}
const chipTexts = (page, layer) => page.locator(`#architecture-diagram [data-trellis-layer-id="${layer}"] > text.trellis-architecture-stat`)
  .evaluateAll(nodes => nodes.map(node => [node.textContent, node.getAttribute("data-tone")]));

// #449: the full stack with allowed arrows dimmed, collapsed bands as metric-coloured chips,
// lower-bound counts with measured coverage, a compact arrow key and counted review chips.
test("Architecture draws the full stack, module chips per band, lower-bound counts with coverage and counted review chips", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  const dto = (await architecture(selected)).data.payload, check = rulesCheck(selected);
  const production = dto.edges.filter(edge => edge.role === "production");
  assert.deepEqual(await bandStates(page), ["low", "mid", "peer", "shared", "top", hostileLayer].map(name => [name, "false"]).sort(),
    "#391: bands without a display hint start collapsed, and every layer is drawn, even one without edges");
  assert.equal(await page.locator("#architecture-deviations-only, #architecture-quiet, #architecture-refresh").count(), 0, "no layer-dropping focus, quiet footer or refresh button");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()),
    production.map(summaryKey).sort(), "allowed arrows stay drawn next to the deviations");
  const tones = await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => Object.fromEntries(nodes.map(node => [node.getAttribute("data-trellis-edge-id"), [node.getAttribute("data-trellis-edge-tone"), node.getAttribute("opacity")]])));
  for (const edge of production) {
    const deviation = edge.status === "deviates" && !edge.excepted;
    assert.deepEqual(tones[summaryKey(edge)], deviation ? ["deviation", null] : edge.status === "unknown" ? ["unknown", null] : ["allowed", "0.75"], summaryKey(edge));
  }
  // Collapsed bands show module chips coloured by metric, not "N files folded".
  const low = '#architecture-diagram [data-trellis-layer-id="low"]';
  const unmeasured = dto.layers.filter(layer => !dto.modules.some(module => module.layer === layer.name && module.declarationCount > 0));
  assert.ok(unmeasured.some(layer => layer.name === hostileLayer));
  assert.equal(await page.locator("#architecture-diagram .pele-node").filter({hasText:/files folded$/}).count(), unmeasured.length, "only a layer without measured modules keeps a file count");
  const lowChips = page.locator('#architecture-diagram [data-trellis-summary-chip="low"]');
  assert.ok(await lowChips.count() >= 2, "the low band shows its module chips while collapsed");
  assert.equal(await lowChips.first().getAttribute("role"), null, "summary chips are display only; the band header is the toggle");
  assert.equal(await page.locator("#architecture-diagram [data-trellis-module-id]").count(), 0, "collapsed bands draw no interactive file chips");
  const lowFill = await page.locator('#architecture-diagram [data-trellis-summary-chip="low"] > rect').first().getAttribute("fill");
  assert.equal(lowFill, "#343644", "summary chips carry the fixed heat band of their module (all low modules are ≤5 cognitive)");
  assert.match(await page.locator(low).getAttribute("aria-label"), /^3 · low · \d+ files/, "the band's one toggle is named by its layer");
  const width = await page.locator("#architecture-diagram").evaluate(node => [node.querySelector("svg").getBoundingClientRect().width, node.clientWidth]);
  // Two shared boxes and a peer row set a minimum width; it either fits or scrolls inside the canvas.
  assert.ok(width[0] >= 0.75 * width[1], `the drawing uses the panel width, not a narrow column (${width[0]} in ${width[1]})`);

  // Lower bounds: low has an unresolved production site, so its counts read ≥ with coverage.
  const lowCoverage = dto.references.coverage.find(row => row.layer === "low" && row.role === "production");
  assert.deepEqual([lowCoverage.mappedSites, lowCoverage.unresolvedSites], [2, 1], "low.rs: two sites map to top.rs; external::unknown stays unresolved");
  const share = `${Math.round(lowCoverage.mappedSites / lowCoverage.sites * 100)}%`;
  const bound = layer => (dto.references.coverage.find(row => row.layer === layer && row.role === "production")?.unresolvedSites ?? 1) > 0;
  const chips = expectedChips(check, dto.layers);
  assert.deepEqual(chips.get("low"), ["↑2"], "the fixture's low layer has a 2-site upward deviation");
  for (const layer of ["low", "mid", "peer", "shared", "top"]) {
    const drawn = await chipTexts(page, layer);
    const expected = (chips.get(layer) || []).map(text => bound(layer) ? text.replace(/(\d+)$/, "≥$1") : text);
    assert.deepEqual(drawn.map(([text]) => text), expected, `${layer} chips match rules check, as lower bounds where sites stayed unresolved`);
    for (const [text, tone] of drawn) assert.equal(tone, /^[↑⇄]/.test(text) ? "error" : "neutral", text);
  }
  const arrow = page.locator("#architecture-diagram").getByRole("button", {name:new RegExp(`^2 production sites · upward\\s+deviation · lower bound; ${share} of ${lowCoverage.sites} production reference sites from low mapped to a module$`)});
  assert.equal(await arrow.locator("text").textContent(), `↑≥2 · ${share} mapped`, "a deviation arrow carries its bound and its source layer's measured coverage");
  assert.equal(await arrow.locator("rect").getAttribute("stroke"), "#FF8E9A", "an upward deviation uses the error colour");
  const coverage = await page.locator("#architecture-coverage").textContent();
  const sites = dto.references.coverage.reduce((sum, row) => sum + row.sites, 0), mapped = dto.references.coverage.reduce((sum, row) => sum + row.mappedSites, 0);
  assert.equal(mapped, dto.references.counts.mappedSites);
  assert.ok(coverage.startsWith(`${mapped} of ${sites} reference sites mapped to a module (`), coverage);
  // #525: the coverage sentence is said once, beside the arrow key; the rule-instance counts sit behind the ⓘ.
  const rulesLine = `${check.counts.unknown} of ${check.counts.comparable} production rule instances unknown`;
  assert.ok(!coverage.includes(rulesLine), coverage);
  assert.ok((await page.locator("#architecture-notice-content").textContent()).includes(rulesLine));
  const said = await page.evaluate(sentence => document.body.innerText.split(sentence).length - 1, coverage.split(" · ")[0]);
  assert.equal(said, 1, "the coverage sentence appears once in the view and inspector");
  assert.match(await page.locator(".architecture-edge-legend").textContent(), /↓ down · allowed.*↑ up · deviation.*≥ lower bound.*test-only/s, "a compact arrow key replaces the prose");

  // Counted review chips rest in the inspector.
  assert.equal(await page.locator("#architecture-inspector").isVisible(), true);
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Architecture review");
  assert.deepEqual(await page.locator("#architecture-inspector-content .architecture-chip").allTextContents(),
    [`${check.counts.deviates} deviation${check.counts.deviates === 1 ? "" : "s"}`, `${check.unassigned.length} unassigned`, "0 exceptions"]);
  await page.locator("#architecture-review").click(); await settled(page);
  assert.equal(await page.locator("#architecture-review-list [data-rule]").count(), check.counts.deviates, "the deviations chip lists exactly what it counts");
  await page.locator("#architecture-review-close").click();
  await capture(page, "architecture-modules-default");

  await page.locator(low).focus();
  await rerendered(page, () => page.keyboard.press("Enter"));
  assert.equal(await page.locator(low).getAttribute("aria-expanded"), "true");
  assert.equal(await focused(page, low), true, "keyboard focus stays on the band after it re-renders");
  const lowModule = dto.modules.find(module => module.identity.path === "src/low.rs");
  const lowGroup = `#architecture-diagram [data-trellis-directory-id='${JSON.stringify(["low", "src/low"])}']`;
  assert.match(await page.locator(lowGroup).getAttribute("aria-label"), /^low\n2 files · max \d+\nsrc\/low$/, "low.rs and low/ form one module chip");
  assert.match(await page.locator(lowGroup).locator("text").textContent(), /^low\d+ · 2 files$/);
  assert.equal(await page.locator(lowGroup).getAttribute("aria-expanded"), "false");
  assert.ok(!(await moduleIds(page)).includes(identityKey(lowModule.identity)), "a module's files stay folded in its chip");
  await page.locator(lowGroup).focus();
  await rerendered(page, () => page.keyboard.press("Enter"));
  assert.equal(await page.locator(lowGroup).getAttribute("aria-expanded"), "true");
  assert.equal(await focused(page, lowGroup), true, "keyboard focus stays on the module after it re-renders");
  const extra = dto.modules.find(module => module.identity.path === "src/low/extra.rs");
  assert.ok((await moduleIds(page)).includes(identityKey(lowModule.identity)) && (await moduleIds(page)).includes(identityKey(extra.identity)), "an open module shows its files");
  await rerendered(page, () => page.keyboard.press("Enter"));
  assert.ok(!(await moduleIds(page)).includes(identityKey(extra.identity)), "the module folds again");

  await page.locator(low).focus();
  await rerendered(page, () => page.keyboard.press("Enter"));
  assert.equal(await page.locator(low).getAttribute("aria-expanded"), "false");
  assert.deepEqual((await chipTexts(page, "low")).map(([text]) => text), ["↑≥2"], "a folded band keeps its count chips");
  await rerendered(page, () => page.keyboard.press("Enter"));
  assert.equal(await page.locator(low).getAttribute("aria-expanded"), "true");
  const stored = await displayStore(selected);
  assert.deepEqual(stored, {open:[["low", true]], drilled:[], unassigned:false, hidden:false});
  await rerendered(page, () => page.locator("#view-architecture").click());
  assert.equal(await page.locator(low).getAttribute("aria-expanded"), "true", "display choices are remembered for this checkout");
  // A click on a display-only summary chip falls through to its band header toggle.
  const midChip = await page.locator('#architecture-diagram [data-trellis-summary-chip="mid"]').first().boundingBox();
  await rerendered(page, () => page.mouse.click(midChip.x + midChip.width / 2, midChip.y + midChip.height / 2));
  assert.equal(await page.locator('#architecture-diagram [data-trellis-layer-id="mid"]').getAttribute("aria-expanded"), "true");
}));

test("Architecture hides test and fixture layers until tests and fixtures are included", {timeout:240000}, async () => {
  const config = `${rules}[layers.fixtures]\norder = 0\npaths = ["tests/fixtures/**"]\n`;
  const extra = new Map([["tests/fixtures/sample.rs", "pub fn sample(value: i32) -> i32 { if value > 0 { value } else { 0 } }\n"]]);
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    const dto = (await architecture(selected)).data.payload;
    assert.equal(dto.modules.find(module => module.layer === "fixtures").excludedReason, "fixture");
    assert.ok(!(await bandStates(page)).some(([name]) => name === "fixtures"), "the fixture layer is not drawn first, or at all, by default");
    assert.match(await page.locator("#architecture-hidden-toggle").textContent(), /1 hidden layer · 1 file/);
    assert.equal(await page.locator("#architecture-hidden-names").textContent(), "fixtures (tests/fixtures; include them to show)");
    await H.reveal(page, "#architecture-metric-tests"); await page.locator("#architecture-metric-tests").check();
    await page.waitForFunction(() => document.querySelector("#architecture-summary")?.textContent.includes("all indexed callables"));
    await ready(page);
    assert.equal(await page.locator("#architecture-hidden").isVisible(), false);
    assert.ok((await bandStates(page)).some(([name]) => name === "fixtures"), "with tests included the edge-free fixture layer is drawn in the stack");
  }, extra);
});

test("Architecture applies authored display hints without changing evaluation", {timeout:240000}, async () => {
  const hinted = rules.replace("[layers.peer]\norder = 2\n", "[layers.peer]\norder = 2\ndisplay = \"hidden\"\n")
    .replace("[layers.top]\norder = 1\n", "[layers.top]\norder = 1\ndisplay = \"expanded\"\n")
    .replace(`[layers.'${hostileLayer}']\nshared = true\n`, `[layers.'${hostileLayer}']\nshared = true\ndisplay = "collapsed"\n`);
  assert.equal((hinted.match(/display = /g) || []).length, 3);
  await published(hinted, async selected => {
    const {page} = selected;
    await ready(page);
    const dto = (await architecture(selected)).data.payload;
    assert.deepEqual(Object.fromEntries(dto.layers.map(layer => [layer.name, layer.display])),
      {low:null, mid:null, peer:"hidden", shared:null, top:"expanded", [hostileLayer]:"collapsed"});
    const hiddenModule = dto.modules.find(row => row.layer === "peer");
    assert.equal(hiddenModule.excludedReason, "hidden_layer"); assert.equal(hiddenModule.metrics.cognitive, null);
    assert.match(await page.locator("#architecture-exclusions").textContent(), /1 authored hidden-layer files/);
    assert.equal(siteTotal(dto.edges.filter(edge => edge.role === "production")), 8, "the hint never changes summary counts");
    assert.equal(dto.references.counts.mappedSites, 13);
    assert.equal(dto.edges.find(edge => edge.role === "production" && edge.sourceLayer === "mid" && edge.destinationLayer === "peer").status, "deviates");
    assert.deepEqual(await bandStates(page), [["low", "false"], ["mid", "false"], ["shared", "false"], ["top", "true"], [hostileLayer, "false"]].sort(), "hidden is in the footer; expanded starts open; collapsed draws a summary band");
    assert.equal(await page.locator("#architecture-hidden-toggle").textContent(), "▸ 1 hidden layer · 1 file · 1 production edge");
    await rerendered(page, () => page.locator("#architecture-hidden-toggle").click());
    assert.ok((await bandStates(page)).some(([name]) => name === "peer"), "a hidden layer is shown from the footer");
    const revealed = (await architecture(selected)).data.payload.modules.find(row => row.layer === "peer");
    assert.equal(revealed.excludedReason, "hidden_layer", "display reveal does not override the metrics population");
    assert.equal(revealed.metrics.cognitive, null);
    assert.ok((await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => JSON.parse(node.getAttribute("data-trellis-edge-id"))))).some(([source, destination]) => source === "mid" && destination === "peer"));
  });
});

const rulesPath = paths => join(paths.workspace, ".trellis/rules.toml");
const rulesPosts = (selected, action) => selected.requests.filter(row => row.method === "POST" && row.url.pathname.endsWith(`/architecture/rules/${action}`));
const rulesResponse = (page, action) => page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname.endsWith(`/architecture/rules/${action}`));
async function previewChange(selected) {
  const before = readFileSync(rulesPath(selected.paths), "utf8");
  const response = rulesResponse(selected.page, "preview");
  await selected.page.locator("#architecture-preview").click();
  const received = await response, data = await received.json();
  assert.equal(received.status(), 200, JSON.stringify(data));
  await selected.page.locator("#architecture-preview-result").waitFor({state:"visible"});
  assert.equal(await selected.page.locator("#architecture-proposed-text").textContent(), data.payload.text);
  assert.equal(await selected.page.locator("#architecture-diff").textContent(), data.payload.diff);
  assert.equal(readFileSync(rulesPath(selected.paths), "utf8"), before, "preview never changes disk");
  assert.notEqual(data.payload.text, before);
  assert.ok(data.payload.diff.includes("@@"), "preview includes an actual unified diff");
  return data.payload;
}
async function confirmChange(selected, preview) {
  const response = rulesResponse(selected.page, "apply");
  await selected.page.locator("#architecture-confirm").click();
  const received = await response, data = await received.json();
  assert.equal(received.status(), 200, JSON.stringify(data));
  assert.equal(data.payload.mutationOutcome, "committed");
  assert.equal(data.payload.rulesFile.contentHash, preview.proposedHash);
  assert.equal(readFileSync(rulesPath(selected.paths), "utf8"), preview.text, "apply writes exactly the preview text");
  await selected.page.waitForFunction(hash => document.querySelector("#architecture-notice-content")?.textContent.includes(hash), preview.proposedHash);
  await ready(selected.page);
  const refreshed = await architecture(selected);
  assert.equal(refreshed.status, 200, JSON.stringify(refreshed.data));
  assert.equal(refreshed.data.payload.indexGeneration, selected.pin.indexGeneration);
  assert.equal(refreshed.data.payload.indexRevision, selected.pin.indexRevision, "rules authoring never publishes a new code snapshot");
  assert.equal(refreshed.data.payload.rulesFile.contentHash, preview.proposedHash);
  return refreshed.data.payload;
}
const reviewPair = (page, source, destination) => page.locator("#architecture-review-list [data-rule]")
  .filter({has:page.getByRole("checkbox", {name:`Select ${source} → ${destination}`, exact:true})});

test("Architecture review keeps exact evidence, independent filters and all unassigned files", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  const dto = (await architecture(selected)).data.payload;
  const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation(?: · .*)?$/});
  // In the summary a skip-level count can sit under a peer count; select it by keyboard.
  await edge.focus(); await page.keyboard.press("Enter");
  await page.locator("#architecture-review-panel").waitFor({state:"visible"}); await settled(page);
  const row = reviewPair(page, "src/low.rs", "src/top.rs");
  assert.equal(await row.count(), 1);
  assert.match(await row.textContent(), /\$layers.*src\/low\.rs.*src\/top\.rs.*deviates/is);
  assert.match(await row.textContent(), /2/);
  const pair = page.locator("#architecture-inspector-content > details").filter({has:page.locator("summary", {hasText:/src\/low\.rs → src\/top\.rs/})}).first();
  await pair.locator(":scope > summary").click();
  assert.match(await pair.textContent(), /2 sampled sites.*0 omitted sites/is);
  const sample = pair.locator(":scope > details").filter({has:page.locator("summary", {hasText:/production · reference/})}).first();
  await sample.locator(":scope > summary").click();
  const sampleText = await sample.textContent();
  assert.match(sampleText, /crate::top::target/);
  assert.match(sampleText, /ownerSyntaxId/);
  await sample.getByRole("button", {name:"Read indexed reference source", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#source .source-line.highlight"));
  assert.match(await page.locator("#source-path").textContent(), /src\/low\.rs.*cached snapshot/is);
  const reference = dto.edges.filter(edge => edge.role === "production").flatMap(edge => edge.pairs).find(value => value.source.path === "src/low.rs" && value.target.path === "src/top.rs");
  assert.equal(await page.locator("#source .source-line.highlight .line-number").first().textContent(), String(reference.sites[0].lines.start));

  await page.locator("#architecture-review").click();
  await page.locator("#architecture-review-role").selectOption("production");
  await page.locator("#architecture-review-state").selectOption("deviates"); await settled(page);
  const production = await page.locator("#architecture-review-list").textContent();
  assert.match(production, /src\/low\.rs.*src\/top\.rs/is);
  assert.match(production, /src\/mid\.rs.*src\/peer\.rs/is);
  await page.locator("#architecture-review-role").selectOption("test"); await settled(page);
  const tests = await page.locator("#architecture-review-list").textContent();
  assert.match(tests, /src\/low\.rs.*src\/top\.rs/is);
  assert.doesNotMatch(tests, /src\/mid\.rs.*src\/peer\.rs/is);
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), false, "review role never changes the heat population");
  assert.equal(await page.locator("#architecture-tests").isChecked(), false, "review role never changes the reference overlay");
  await page.locator("#architecture-review-role").selectOption("all");
  await page.locator("#architecture-review-state").selectOption("unknown"); await settled(page);
  assert.match(await page.locator("#architecture-review-list").textContent(), /unknown/i);
  assert.equal(await page.locator("#architecture-review-list input[type=checkbox]:enabled").count(), 0, "unknown destinations cannot be waived");
  await page.locator("#architecture-unassigned").click();
  const unassigned = await page.locator("#architecture-review-list").textContent();
  assert.ok(dto.modules.filter(module => module.layer === null).every(module => unassigned.includes(module.identity.path)));
  assert.ok(unassigned.includes("src/only.rs"), "a no-edge file is still listed for assignment");
  assert.ok(unassigned.includes("src/unassigned.rs"));
  await page.setViewportSize({width:390, height:844});
  await noOverflow(page);
}));

test("Architecture groups two witnessed pairs in one reason, previews once and adds/edits/removes exact bytes", {timeout:240000}, async () => published(`# preserved rules header\n${rules}`, async selected => {
  const {page} = selected;
  await ready(page);
  await page.locator("#architecture-review").click();
  await page.locator("#architecture-review-role").selectOption("production");
  await page.locator("#architecture-review-state").selectOption("deviates"); await settled(page);
  for (const [source, destination] of [["src/low.rs", "src/top.rs"], ["src/mid.rs", "src/peer.rs"]]) {
    await reviewPair(page, source, destination).getByRole("checkbox").check();
  }
  await page.locator("#architecture-add-selected").click();
  await page.locator("#architecture-reason").fill("   ");
  await page.locator("#architecture-preview").click();
  assert.equal(rulesPosts(selected, "preview").length, 0, "blank reason is rejected before any preview request");
  const reason = '<img src=x onerror="globalThis.ruleReasonExecuted=true"> reviewed two concrete pairs';
  await page.locator("#architecture-reason").fill(reason);
  const preview = await previewChange(selected);
  assert.equal(preview.pairs.length, 2);
  assert.equal(rulesPosts(selected, "preview").length, 1);
  const body = rulesPosts(selected, "preview")[0].body;
  assert.equal(body.operations.length, 1);
  assert.equal(body.operations[0].type, "addExceptions");
  assert.equal(body.operations[0].reason, reason);
  assert.deepEqual(body.operations[0].pairs.map(pair => [pair.source.path, pair.destination.path]).sort(), [["src/low.rs", "src/top.rs"], ["src/mid.rs", "src/peer.rs"]].sort());
  assert.ok(preview.text.startsWith("# preserved rules header\n"), "unrelated authored text is preserved");
  await page.locator("#architecture-editor").scrollIntoViewIfNeeded();
  await capture(page, "architecture-grouped-preview-desktop");
  await page.setViewportSize({width:390, height:844});
  await page.locator("#architecture-preview-result").scrollIntoViewIfNeeded();
  await capture(page, "architecture-grouped-preview-mobile");
  await page.setViewportSize({width:1440, height:1000});
  const dto = await confirmChange(selected, preview);
  assert.equal(rulesPosts(selected, "apply").length, 1, "grouped authoring is one apply, never one save per pair");
  const applied = rulesPosts(selected, "apply")[0].body;
  assert.deepEqual(applied.operations, body.operations);
  assert.equal(applied.baseHash, body.baseHash);
  assert.equal(applied.proposedHash, preview.proposedHash);
  for (const [source, destination] of [["src/low.rs", "src/top.rs"], ["src/mid.rs", "src/peer.rs"]]) {
    const instance = rulesCheck(selected).instances.find(row => row.rule === "$layers" && row.target.path === source && row.destination?.path === destination);
    assert.equal(instance.excepted, true);
    assert.equal(instance.status, "deviates", "excepted keeps the underlying verdict");
    assert.equal(instance.exceptionReason, reason);
  }
  assert.equal(await page.evaluate(() => globalThis.ruleReasonExecuted), undefined);
  assert.equal(await page.locator("#architecture-review-panel img, #architecture-editor img").count(), 0);

  await page.locator("#architecture-review").click();
  await page.locator("#architecture-review-state").selectOption("excepted"); await settled(page);
  assert.ok((await page.locator("#architecture-review-list").textContent()).includes(reason));
  await page.locator("#architecture-authored").click();
  const exception = page.locator("#architecture-exceptions").getByRole("button", {name:"Edit reason", exact:true}).first();
  await exception.click();
  await page.locator("#architecture-reason").fill("Updated reviewed reason");
  const edited = await previewChange(selected);
  await confirmChange(selected, edited);
  assert.ok(rulesCheck(selected).instances.some(row => row.excepted && row.exceptionReason === "Updated reviewed reason"));
  await page.locator("#architecture-review").click();
  await page.locator("#architecture-authored").click();
  await page.locator("#architecture-exceptions").getByRole("button", {name:"Remove", exact:true}).first().click();
  const removed = await previewChange(selected);
  await confirmChange(selected, removed);
  assert.equal(rulesCheck(selected).instances.filter(row => row.rule === "$layers" && row.excepted).length, 1, "removal changes only the chosen entry");
}));



test("Architecture preserves a conflicted draft, reloads explicitly and requires a new preview", {timeout:240000}, async () => published(rules, async selected => {
  const {page, paths} = selected;
  await ready(page);
  await page.locator("#architecture-review").click();
  await page.locator("#architecture-review-role").selectOption("production"); await settled(page);
  await reviewPair(page, "src/low.rs", "src/top.rs").getByRole("checkbox").check();
  await page.locator("#architecture-add-selected").click();
  const reason = "Keep this draft through an ordinary editor conflict";
  await page.locator("#architecture-reason").fill(reason);
  const initial = await previewChange(selected);
  const external = `${rules}\n# ordinary external editor changed disk after preview\n`;
  writeFileSync(rulesPath(paths), external);
  const response = rulesResponse(page, "apply");
  await page.locator("#architecture-confirm").click();
  const received = await response, data = await received.json();
  assert.equal(received.status(), 409, JSON.stringify(data));
  assert.equal(data.error.code, "rules_conflict");
  assert.equal(readFileSync(rulesPath(paths), "utf8"), external, "conflict never overwrites the external edit");
  assert.equal(await page.locator("#architecture-reason").inputValue(), reason);
  assert.match(await page.locator("#architecture-editor").textContent(), /rules_conflict|changed|conflict/i);
  assert.equal(rulesPosts(selected, "apply").length, 1, "conflict does not auto-retry");
  await page.locator("#architecture-rules-reload").click();
  await page.waitForFunction(hash => {
    const notice = document.querySelector("#architecture-notice-content")?.textContent;
    return notice && !notice.includes(hash);
  }, initial.baseHash);
  await ready(page);
  assert.equal(await page.locator("#architecture-reason").inputValue(), reason, "Reload keeps the reason but invalidates the old preview");
  assert.equal(await page.locator("#architecture-confirm").isVisible(), false);
  const rebased = await previewChange(selected);
  assert.notEqual(rebased.baseHash, initial.baseHash);
  assert.ok(rebased.text.includes("# ordinary external editor changed disk after preview"));
  assert.equal(rulesPosts(selected, "preview").length, 2, "the retained draft needs a new explicit preview");
  const dto = await confirmChange(selected, rebased);
  assert.equal(rulesPosts(selected, "apply").length, 2);
  assert.ok(rulesCheck(selected).instances.some(row => row.target.path === "src/low.rs" && row.destination?.path === "src/top.rs" && row.excepted));
}));



// #527: the summary is cached per revision, but rules are read live. A rules_conflict from a
// detail read must make "Reload Architecture" read the current rules, not the cached summary.
test("Architecture Reload after a detail rules_conflict reads the current rules, not the cached summary", {timeout:240000}, async () => published(rules, async selected => {
  const {page, paths, requests} = selected;
  await ready(page); await expandAll(page);
  const summaries = () => requests.filter(row => row.url.pathname.endsWith("/architecture")).length;
  const before = summaries();
  writeFileSync(rulesPath(paths), `${rules}\n# ordinary external editor changed disk at the same revision\n`);
  const statuses = [];
  page.on("response", response => { if (new URL(response.url()).pathname.endsWith("/architecture/details")) statuses.push(response.status()); });
  await page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/}).click();
  const reload = page.locator("#architecture-inspector").getByRole("button", {name:"Reload Architecture", exact:true});
  await reload.first().waitFor({timeout:30000}).catch(async error => assert.fail(`${error.message}: details ${JSON.stringify(statuses)} inspector ${await page.locator("#architecture-inspector").textContent()}`));
  assert.ok(statuses.includes(409), `a detail read refused the old rules hash: ${JSON.stringify(statuses)}`);
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /Rules changed since this diagram was loaded/);
  await reload.first().click();
  for (const deadline = Date.now() + 60000; summaries() === before; await page.waitForTimeout(50)) assert.ok(Date.now() < deadline, "Reload Architecture reads the summary again");
  await ready(page); await expandAll(page);
  const details = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/architecture/details"));
  await page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/}).click();
  assert.equal((await details).status(), 200, "the reloaded summary carries the current rules hash");
  await settled(page);
  assert.doesNotMatch(await page.locator("#architecture-inspector-content").textContent(), /Rules changed since this diagram was loaded/);
}));

test("Architecture assigns a no-edge unassigned file through exact preview without indexing", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  await page.locator("#architecture-unassigned").click();
  const row = page.locator("#architecture-review-list .architecture-rule-row").filter({has:page.getByRole("combobox", {name:"Layer for src/only.rs", exact:true})});
  await page.getByRole("combobox", {name:"Layer for src/only.rs", exact:true}).selectOption("top");
  await row.getByRole("button", {name:"Assign file", exact:true}).click();
  const preview = await previewChange(selected);
  const operation = rulesPosts(selected, "preview")[0].body.operations;
  assert.deepEqual(operation, [{type:"assignFile", path:"src/only.rs", layer:"top"}]);
  const dto = await confirmChange(selected, preview);
  assert.equal(dto.modules.find(module => module.identity.path === "src/only.rs").layer, "top");
  assert.equal(rulesPosts(selected, "preview").length, 1);
  assert.equal(rulesPosts(selected, "apply").length, 1);
  await page.locator("#architecture-unassigned").click();
  assert.equal(await page.getByRole("combobox", {name:"Layer for src/only.rs", exact:true}).count(), 0);
  assert.equal(await page.getByRole("combobox", {name:"Layer for src/unassigned.rs", exact:true}).count(), 1, "other unassigned modules are untouched");
}));



test("Architecture edits and removes an orphan named-rule exception without any layers", {timeout:240000}, async () => {
  const config = `version = 1
# Keep this named rule even though no current declaration selects it.
[roles.orphan]
select = { paths = ["src/absent.rs"] }
[[rules]]
id = "orphan-review"
for_each = "orphan"
require = [{ calls = "orphan" }]
[[exceptions]]
rule = "orphan-review"
target = { source_set = "orphan-source-set", language = "rust", path = "src/absent.rs", sid = "sid:v1:00000000000000000000000000000000" }
reason = "Original orphan reason" # keep reason comment
`;
  await published(config, async selected => {
    const {page, paths} = selected;
    await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("No authored layers"));
    const before = await architecture(selected);
    assert.equal(before.status, 200, JSON.stringify(before.data));
    assert.equal(before.data.payload.state, "no_layers");
    assert.equal(await page.locator("#architecture-diagram svg").count(), 0);
    await page.locator("#architecture-authored").click();
    const entry = page.locator("#architecture-exceptions [data-exception-index='0']");
    await entry.waitFor({state:"visible"});
    assert.match(await entry.textContent(), /orphan-review.*src\/absent\.rs.*Original orphan reason/is);
    await entry.getByRole("button", {name:"Edit reason", exact:true}).click();
    await page.locator("#architecture-reason").fill("Reviewed orphan reason remains editable");
    const edited = await previewChange(selected);
    assert.ok(edited.text.includes("# keep reason comment"), "reason edit keeps the attached comment");
    assert.ok(edited.text.includes("# Keep this named rule"));
    async function applyWithoutDiagram(preview) {
      const response = rulesResponse(page, "apply");
      await page.locator("#architecture-confirm").click();
      const received = await response, data = await received.json();
      assert.equal(received.status(), 200, JSON.stringify(data));
      assert.equal(data.payload.mutationOutcome, "committed");
      assert.equal(readFileSync(rulesPath(paths), "utf8"), preview.text);
      await page.locator("#architecture-editor").waitFor({state:"hidden"});
      await page.waitForFunction(hash => document.querySelector("#architecture-notice-content")?.textContent.includes(hash), preview.proposedHash);
      await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Rules committed. Rechecked"));
      const refreshed = await architecture(selected);
      assert.equal(refreshed.status, 200, JSON.stringify(refreshed.data));
      assert.equal(refreshed.data.payload.state, "no_layers");
      assert.equal(refreshed.data.payload.rulesFile.contentHash, preview.proposedHash);
      assert.equal(refreshed.data.payload.indexGeneration, selected.pin.indexGeneration);
      assert.equal(refreshed.data.payload.indexRevision, selected.pin.indexRevision);
      assert.equal(await page.locator("#architecture-diagram svg").count(), 0);
    }
    await applyWithoutDiagram(edited);
    await page.locator("#architecture-authored").click();
    await entry.waitFor({state:"visible"});
    assert.match(await entry.textContent(), /Reviewed orphan reason remains editable/);
    await entry.getByRole("button", {name:"Remove", exact:true}).click();
    const removed = await previewChange(selected);
    assert.ok(!removed.text.includes("[[exceptions]]"));
    assert.ok(removed.text.includes('id = "orphan-review"'), "removing an orphan exception keeps the named rule");
    await applyWithoutDiagram(removed);
    await page.locator("#architecture-authored").click();
    await page.waitForFunction(() => document.querySelector("#architecture-exceptions")?.textContent.includes("No authored exceptions"));
    assert.equal(rulesPosts(selected, "preview").length, 2);
    assert.equal(rulesPosts(selected, "apply").length, 2);
  });
});

// Setup/error states use separate pre-start configurations, never racing live edits.

for (const [name, config, state] of [["missing", null, "missing_rules"], ["empty", "version = 1\n", "no_layers"]]) {
  test(`Architecture ${name} rules show proposal help without inventing a diagram`, {timeout:240000}, async () => published(config, async selected => {
    const response = await architecture(selected);
    assert.equal(response.status, 200, JSON.stringify(response.data));
    assert.equal(response.data.payload.state, state);
    assert.deepEqual(response.data.payload.modules, []); assert.deepEqual(response.data.payload.edges, []);
    await selected.page.waitForFunction(expected => document.querySelector("#architecture-state")?.textContent.includes(expected), state === "no_layers" ? "No authored layers" : "trellis_propose_layers");
    const message = await selected.page.locator("#architecture-panel").textContent();
    assert.match(message, /\.trellis\/rules\.toml/);
    assert.match(message, state === "no_layers" ? /Existing authored exceptions/ : /trellis_propose_layers/);
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

// #450: Libraries is no tab; a reference into a catalogued external package opens it in context.
// A registry dependency is a proven external package; its source comes from the local
// Cargo cache, which the catalog reads only on refresh.
const libraryFiles = new Map([
  ["Cargo.toml", "[package]\nname='browser-app'\nversion='0.1.0'\n[dependencies]\nlocal-library='0.1'\n"],
  ["Cargo.lock", "version=4\n[[package]]\nname='browser-app'\nversion='0.1.0'\ndependencies=['local-library']\n[[package]]\nname='local-library'\nversion='0.1.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n"],
  ["src/uses_library.rs", "use local_library::candidate_only;\npub fn uses_library() { candidate_only(); }\n"],
  ["src/lib.rs", `${sources.get("src/lib.rs")}pub mod uses_library;\n`],
]);
test("Architecture opens an external package reference in the library catalog, without a Libraries tab", {timeout:240000}, async () => published(rules, async selected => {
  const {page, paths, server, requests} = selected;
  assert.equal(await page.locator("#view-libraries, #dependency-library").count(), 0, "the Libraries tab and its route are gone");
  assert.equal(await page.locator("#dock-library").isVisible(), false, "no library dock tab before an in-context open");
  const cached = join(paths.cargoHome, "registry/src/cache/local-library-0.1.0");
  mkdirSync(join(cached, "src"), {recursive:true});
  writeFileSync(join(cached, "Cargo.toml"), "[package]\nname='local-library'\nversion='0.1.0'\n");
  writeFileSync(join(cached, "src/lib.rs"), "// <img src=x onerror='globalThis.libraryExecuted=true'>\npub fn candidate_only() {}\n");
  // Serve registration can leave the automatic catalog loading; request it and wait.
  const started = await H.api(server.base, paths.token, "POST", H.scoped(server.rootKey, "dependencies/refresh"), {});
  assert.equal(started.status, 202, JSON.stringify(started.data));
  for (const deadline = Date.now() + 30000; ;) {
    const catalog = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "dependencies"));
    if (catalog.data.state === "ready") { assert.ok(catalog.data.packages.some(pkg => pkg.name === "local-library"), JSON.stringify(catalog.data)); break; }
    assert.ok(Date.now() < deadline && !["failed", "disabled"].includes(catalog.data.state), JSON.stringify(catalog.data));
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  // #527: the page reads the catalog only when an in-context entry needs it.
  assert.equal(requests.some(row => row.url.pathname.endsWith("/dependencies")), false, "no eager catalog read");
  const catalogRead = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/dependencies"));
  await page.locator("#architecture-notices > summary").click();
  await page.getByRole("button", {name:"Inspect same-layer and unassigned references", exact:true}).click(); await settled(page);
  assert.equal((await (await catalogRead).json()).state, "ready");
  const pair = page.locator("#architecture-inspector-content details").filter({has:page.locator(":scope > summary", {hasText:/src\/uses_library\.rs → local[-_]library \(rust package\)/})}).first();
  await pair.locator(":scope > summary").click();
  const open = pair.getByRole("button", {name:"Open in library", exact:true}); await open.waitFor();
  assert.equal(await open.count(), 1);
  assert.equal(await page.locator("#architecture-inspector-content button").filter({hasText:"Open in library"}).count(), 1, "only the catalogued package offers the library");
  await open.click();
  await page.locator("#library-source-panel").waitFor();
  assert.equal(await page.locator("#dock-library").getAttribute("aria-selected"), "true");
  await page.waitForFunction(() => /local-library 0\.1\.0.*Architecture reference/.test(document.querySelector("#dependency-state")?.textContent || ""));
  const definition = page.locator("#dependency-symbols button").filter({hasText:"candidate_only"});
  await definition.waitFor();
  await definition.click();
  await page.waitForFunction(() => /immutable candidate snapshot/.test(document.querySelector("#external-source-path")?.textContent || ""));
  assert.match(await page.locator("#external-source").textContent(), /pub fn candidate_only\(\) \{\}/);
  assert.equal(await page.locator("#external-source img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.libraryExecuted), undefined);
  assert.match(await page.locator("#external-source-hint").textContent(), /Workspace diagrams are unavailable/);
  assert.equal(await page.locator("#external-source .source-diagram-action").count(), 0);
  await page.locator("#external-source").focus(); await page.keyboard.press("Shift+F10");
  assert.equal(await page.locator(".classes-context-menu").count(), 0, "external source has no workspace action menu");
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#architecture-inspector").isVisible(), true, "the reference inspection stays open");
  assert.ok(!requests.some(row => /\/(sequence|navigation)$/.test(row.url.pathname)), "library candidates never seed workspace sequences");
}, libraryFiles));

// #525: layout bounds of the drawn diagram, in page pixels. Count tags may cross
// connector strokes and band borders, but never text, a stat chip, a chip or another tag.
function layoutBounds() {
  const canvas = document.querySelector("#architecture-diagram"), svg = canvas.querySelector("svg");
  const box = node => { const r = node.getBoundingClientRect(); return {x:r.x, y:r.y, w:r.width, h:r.height}; };
  const hit = (a, b) => a.x < b.x + b.w - 0.5 && b.x < a.x + a.w - 0.5 && a.y < b.y + b.h - 0.5 && b.y < a.y + a.h - 0.5;
  const tags = [...svg.querySelectorAll(".pele-edge-label")].map(node => ({kind:"tag", text:node.textContent, ...box(node)}));
  const stats = [...svg.querySelectorAll("text.trellis-architecture-stat")].map(node => ({kind:"stat", text:node.textContent, ...box(node.previousElementSibling)}));
  const lines = [...svg.querySelectorAll(".pele-cluster-label, .pele-cluster-label tspan, .pele-label, .pele-label tspan")]
    .filter(node => !node.querySelector("tspan") && node.textContent.trim()).map(node => ({kind:"text", text:node.textContent, ...box(node)}));
  const chips = [...svg.querySelectorAll(".pele-node > rect")].map(node => ({kind:"chip", text:node.parentElement.textContent, ...box(node)}));
  const overlaps = [];
  for (const tag of tags) for (const other of [...stats, ...lines, ...chips, ...tags]) if (other !== tag && hit(tag, other)) overlaps.push(`${tag.text} × ${other.kind}:${other.text}`);
  // Each tag stays beside its own arrow: its box is near some point of its connector.
  const strays = [];
  for (const label of svg.querySelectorAll(".pele-edge-label")) {
    const line = [...svg.querySelectorAll(".pele-edge")].find(edge => edge.getAttribute("data-id") === label.getAttribute("data-id"))?.querySelector("path");
    const r = box(label), length = line.getTotalLength(), matrix = line.getScreenCTM();
    let nearest = Infinity;
    for (let at = 0; at <= length; at += 2) {
      const p = line.getPointAtLength(at), x = matrix.a * p.x + matrix.e, y = matrix.d * p.y + matrix.f;
      nearest = Math.min(nearest, Math.hypot(Math.max(r.x - x, 0, x - r.x - r.w), Math.max(r.y - y, 0, y - r.y - r.h)));
    }
    if (nearest > 200) strays.push(`${label.textContent} ${Math.round(nearest)}px`);
  }
  // A stat chip never sits on header text, nor on another stat chip.
  for (const stat of stats) for (const other of [...lines, ...stats]) if (other !== stat && hit(stat, other)) overlaps.push(`${stat.text} × ${other.kind}:${other.text}`);
  const style = getComputedStyle(canvas), inner = canvas.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  const clipped = [...svg.querySelectorAll("text, tspan")].filter(node => !node.querySelector("tspan") && node.textContent.includes("…")).map(node => node.textContent);
  // Each drawn line fits inside its own node or band box.
  const spills = [];
  for (const group of svg.querySelectorAll(".pele-node, .pele-cluster")) {
    const frame = box(group.querySelector(":scope > rect"));
    for (const line of group.querySelectorAll(":scope > text tspan, :scope > text")) {
      if (line.querySelector("tspan") || !line.textContent.trim()) continue;
      const r = box(line);
      if (r.x < frame.x - 0.5 || r.x + r.w > frame.x + frame.w + 0.5) spills.push(line.textContent);
    }
  }
  const legend = document.querySelector("#architecture-legend").getBoundingClientRect(), drawn = canvas.getBoundingClientRect();
  return {overlaps, strays, clipped, spills, svgWidth:svg.getBoundingClientRect().width, inner, pageFits:document.documentElement.scrollWidth <= innerWidth,
    legendBelow:legend.top >= drawn.bottom - 0.5, stateShown:!!document.querySelector("#architecture-state").offsetParent};
}
async function laidOutAt(page, width) {
  const before = await page.evaluate(() => document.querySelector("#architecture-diagram").clientWidth);
  await page.evaluate(() => { const svg = document.querySelector("#architecture-diagram svg"); if (svg) svg.__old = true; });
  await page.setViewportSize({width, height:1000});
  const after = await page.evaluate(() => document.querySelector("#architecture-diagram").clientWidth);
  if (Math.abs(after - before) >= 8) await page.waitForFunction(() => { const svg = document.querySelector("#architecture-diagram svg"); return svg && !svg.__old; });
  await quiet(page);
}
// The panel can settle in more than one step (the inspector column follows the window).
// The canvas publishes a pending re-layout and the width it last drew for; the drawing is
// final once nothing is pending and that width is within the re-layout slack of the canvas.
async function quiet(page) {
  await page.waitForFunction(() => {
    const canvas = document.querySelector("#architecture-diagram");
    return canvas.dataset.layout !== "pending" && Math.abs(Number(canvas.dataset.layoutWidth) - canvas.clientWidth) < 8;
  });
}
function assertLayout(bounds, label) {
  assert.deepEqual(bounds.overlaps, [], `${label}: count tags and header chips never overlap text, chips or each other`);
  assert.deepEqual(bounds.strays, [], `${label}: every count tag stays beside its own arrow`);
  assert.deepEqual(bounds.clipped, [], `${label}: no title, chip or tag is cut short`);
  assert.deepEqual(bounds.spills, [], `${label}: every line fits its box`);
  assert.ok(bounds.svgWidth <= bounds.inner + 0.5, `${label}: the drawing fits the panel (${bounds.svgWidth} in ${bounds.inner})`);
  assert.ok(bounds.svgWidth >= 0.9 * bounds.inner, `${label}: the drawing uses the panel width (${bounds.svgWidth} in ${bounds.inner})`);
  assert.equal(bounds.pageFits, true, `${label}: no page-level horizontal scroll`);
}
const longModule = "src/a_rather_long_module_name_that_wraps.rs";
const layoutRules = rules.replace('"src/top.rs",', `"src/top.rs", "${longModule}",`)
  .replace("[layers.peer]\norder = 2\n", "[layers.peer]\norder = 2\ndisplay = \"hidden\"\n");
const layoutFiles = new Map([[longModule, "pub fn wraps(v: i32) -> i32 { if v > 0 { if v > 1 { if v > 2 { return crate::low::low(); } } } 0 }\n"]]);

test("Architecture lays out tags, titles and chips without collisions or truncation at 1440 and 900, with one coverage sentence", {timeout:240000}, async () => published(layoutRules, async selected => {
  const {page} = selected;
  await ready(page);
  for (const width of [1440, 900]) {
    await laidOutAt(page, width);
    await ready(page);
    const bounds = await page.evaluate(layoutBounds);
    assertLayout(bounds, `${width}px`);
    assert.equal(bounds.legendBelow, true, `${width}px: the heat legend sits with the arrow key under the drawing`);
    assert.equal(bounds.stateShown, false, `${width}px: no status or count line above the drawing`);
    // Long names wrap at separators instead of clipping; the whole name is drawn.
    const chip = page.locator('#architecture-diagram [data-trellis-summary-chip="top"]').filter({hasText:/a_rather/});
    assert.equal(await chip.count(), 1, "the hottest top module is drawn as a summary chip");
    const drawnLines = await chip.locator("tspan").allTextContents();
    assert.ok(drawnLines.length >= 3, `the long module name wraps onto more lines: ${JSON.stringify(drawnLines)}`);
    assert.ok(drawnLines.join("").startsWith("a_rather_long_module_name_that_wraps"), JSON.stringify(drawnLines));
    // The hidden-layer note is part of the layout: inside the canvas, directly under the drawing.
    const note = await page.evaluate(() => {
      const note = document.querySelector("#architecture-hidden"), svg = document.querySelector("#architecture-diagram svg");
      return {inside:note.parentElement?.id, below:note.getBoundingClientRect().top >= svg.getBoundingClientRect().bottom - 0.5, visible:!note.hidden};
    });
    assert.deepEqual(note, {inside:"architecture-diagram", below:true, visible:true});
  }
  // The coverage sentence is said once; the counts are behind the ⓘ.
  const coverage = (await page.locator("#architecture-coverage").textContent()).split(" · ")[0];
  assert.match(coverage, /^\d+ of \d+ reference sites mapped to a module/);
  assert.equal(await page.evaluate(sentence => document.body.innerText.split(sentence).length - 1, coverage), 1);
  assert.match(await page.locator("#architecture-summary").textContent(), /file modules · \d+ production \/ \d+ test-only cross-layer sites/);
  assert.equal(await page.locator("#architecture-summary").isVisible(), false, "the counts wait behind the ⓘ");
  await page.locator("#architecture-notices > summary").click();
  assert.equal(await page.locator("#architecture-summary").isVisible(), true);
  assert.match(await page.locator("#architecture-notice-content").textContent(), /production rule instances unknown/);
  await capture(page, "architecture-layout-900");
}, layoutFiles));

test("Architecture re-lays out on a panel width change, keeping the selection and the inspector", {timeout:240000}, async () => published(layoutRules, async selected => {
  const {page} = selected;
  await ready(page);
  const tag = page.locator("#architecture-diagram [data-trellis-edge-tone=deviation]").first();
  const edgeId = await tag.getAttribute("data-trellis-edge-id");
  await tag.click(); await settled(page);
  const title = await page.locator("#architecture-inspector-title").textContent();
  assert.match(title, / → /);
  const widthOf = () => page.evaluate(() => document.querySelector("#architecture-diagram svg").getBoundingClientRect().width);
  const wide = await widthOf();
  const requests = selected.requests.length;
  await tag.focus();
  await laidOutAt(page, 1100);
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-trellis-edge-id")), edgeId, "keyboard focus returns to the same arrow");
  const narrow = await widthOf();
  assert.ok(narrow < wide, `the drawing narrows with the window (${narrow} < ${wide})`);
  assertLayout(await page.evaluate(layoutBounds), "1100px");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), title, "the inspection survives the re-layout");
  const pressed = await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll((nodes, id) => nodes.filter(node => node.getAttribute("data-trellis-edge-id") === id).map(node => node.getAttribute("aria-pressed")), edgeId);
  assert.deepEqual(pressed, ["true"], "the selected arrow stays selected");
  assert.equal(selected.requests.slice(requests).filter(row => row.url.pathname.endsWith("/architecture")).length, 0, "a re-layout re-reads nothing");
  // Hiding the inspector widens the panel: the drawing grows to use it.
  await page.evaluate(() => { document.querySelector("#architecture-diagram svg").__old = true; });
  await page.locator("#inspector-collapse").click();
  await page.waitForFunction(() => { const svg = document.querySelector("#architecture-diagram svg"); return svg && !svg.__old; });
  await quiet(page);
  assert.ok(await widthOf() > narrow, "the drawing widens when the inspector is hidden");
  assertLayout(await page.evaluate(layoutBounds), "inspector hidden");
}, layoutFiles));

// #524: developer tooling is not runtime architecture. An authored display="hidden" on the
// tooling layer (the owner's 2026-10-31 ruling) joins it to the fixture layer behind the one
// hidden-layer toggle, and neither layer feeds the Sequence start list or the Classes gate.
const toolingPython = `def generate(values):
    total = 0
    for v in values:
        if v > 0:
            for w in values:
                if w > v:
                    if w % 2:
                        total += w
                    else:
                        total -= w
    return total
`;
const fixtureSource = "pub fn sample(v: i32) -> i32 { if v > 0 { if v > 1 { if v > 2 { if v > 3 { return 4; } } } } 0 }\n";
const toolingLayer = hint => `[layers.tooling]\norder = 1\n${hint ? 'display = "hidden"\n' : ""}paths = ["tools/**"]\n`;
async function startPaths(page) {
  await page.locator("#view-sequence").click();
  const starts = page.locator("#sequence-start-list button");
  await starts.first().waitFor();
  return starts.evaluateAll(nodes => nodes.map(node => node.querySelector(".detail").textContent.split(" · ")[0]));
}
test("Architecture hides tooling with fixtures behind one toggle; the Sequence start list skips both", {timeout:240000}, async () => {
  const config = `${rules}${toolingLayer(true)}[layers.fixtures]\norder = 0\npaths = ["tests/fixtures/**"]\n`;
  await published(config, async selected => {
    const {page, server, paths, pin} = selected;
    await ready(page);
    const drawn = (await bandStates(page)).map(([name]) => name);
    assert.ok(!drawn.includes("tooling") && !drawn.includes("fixtures"), "tooling and fixtures are hidden by default");
    assert.match(await page.locator("#architecture-hidden-toggle").textContent(), /^▸ 2 hidden layers · 2 files/);
    assert.equal(await page.locator("#architecture-hidden-names").textContent(), "fixtures (tests/fixtures; include them to show), tooling");
    await page.locator("#architecture-hidden-toggle").focus();
    await rerendered(page, () => page.keyboard.press("Enter"));
    const shown = (await bandStates(page)).map(([name]) => name);
    assert.ok(shown.includes("tooling") && shown.includes("fixtures"), "the one toggle shows both");
    assert.equal(await page.evaluate(() => document.activeElement?.id), "architecture-hidden-toggle", "the note keeps keyboard focus after it re-lays out");
    // Without the application-layer rule, tooling and fixture code would lead the list.
    const query = new URLSearchParams({scope:"revision", aggregate:"max", role:"all", workbench:"true", topCognitive:"10", indexGeneration:pin.indexGeneration, indexRevision:String(pin.indexRevision)});
    const all = (await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, `metrics?${query}`))).data.payload.topCognitive;
    assert.deepEqual(all.slice(0, 2).map(row => row.path).sort(), ["tests/fixtures/sample.rs", "tools/gen.py"]);
    const starts = await startPaths(page);
    assert.ok(starts.length >= 2, JSON.stringify(starts));
    assert.ok(starts.every(path => path.startsWith("src/")), `start list holds application layers only: ${JSON.stringify(starts)}`);
    // Including tests and fixtures in the metrics changes the heat, not the start list.
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => /all indexed callables/.test(document.querySelector("#metrics-state")?.textContent || ""));
    const included = await page.locator("#sequence-start-list button").evaluateAll(nodes => nodes.map(node => node.querySelector(".detail").textContent.split(" · ")[0]));
    assert.deepEqual(included, starts);
    // Classes stays available from the application's own Rust code (#452), not from tools/.
    await page.locator("#view-classes").waitFor({state:"visible"});
  }, new Map([["tools/gen.py", toolingPython], ["tests/fixtures/sample.rs", fixtureSource]]));
});

test("Classes counts application layers only: tools/*.py alone does not qualify a checkout", {timeout:240000}, async () => {
  const app = new Map([["src/lib.rs", "#![cfg(test)]\nfn only(value: i32) -> i32 { value }\n"]]);
  const config = hint => `version = 1\n[layers.app]\norder = 2\npaths = ["src/**"]\n${toolingLayer(hint)}`;
  const tools = new Map([["tools/gen.py", toolingPython]]);
  await published(config(true), async ({page, requests}) => {
    await page.waitForFunction(() => /Indexed snapshot/.test(document.querySelector("#metrics-state")?.textContent || ""));
    // Including tests and fixtures in the heat reads the population again; the gate keeps the application's.
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => /all indexed callables/.test(document.querySelector("#metrics-state")?.textContent || ""));
    const reads = requests.filter(row => row.url.pathname.endsWith("/metrics") && row.url.searchParams.get("scope") === "revision").map(row => row.url.searchParams.get("role"));
    assert.deepEqual(reads, ["production", "all"], "the application population is known before the gate decides");
    // With no application language known, the class catalog is never asked: a deterministic signal, not a wait.
    assert.equal(requests.some(row => row.url.pathname.endsWith("/classes")), false, "tools/*.py alone does not qualify the checkout");
    assert.equal(await page.locator("#view-classes").isVisible(), false, "Python in the hidden tooling layer earns no Classes tab");
    await page.locator("#view-sequence").click();
    assert.equal(await page.locator("#sequence-start").isVisible(), false, "no application callable to start from, tests included or not");
  }, tools, app);
  await published(config(false), async ({page}) => {
    await page.locator("#view-classes").waitFor({state:"visible"});
  }, tools, app);
});

// Live rules can hide a layer at the same code pin: an explicit Refresh re-reads the
// application population even while tests and fixtures are included in the heat.
test("An explicit Refresh re-reads the application layers after a same-pin rules change, tests included", {timeout:240000}, async () => {
  await published(`${rules}${toolingLayer(false)}`, async ({page, paths, requests}) => {
    await page.waitForFunction(() => /Indexed snapshot/.test(document.querySelector("#metrics-state")?.textContent || ""));
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => /all indexed callables/.test(document.querySelector("#metrics-state")?.textContent || ""));
    const before = await startPaths(page);
    assert.equal(before[0], "tools/gen.py", "visible tooling is an application layer and leads the list");
    const config = readFileSync(rulesPath(paths), "utf8");
    writeFileSync(rulesPath(paths), config.replace("[layers.tooling]\norder = 1\n", "[layers.tooling]\norder = 1\ndisplay = \"hidden\"\n"));
    const revisionReads = () => requests.filter(row => row.url.pathname.endsWith("/metrics") && row.url.searchParams.get("scope") === "revision").length;
    const reads = revisionReads();
    const production = page.waitForResponse(response => {
      const url = new URL(response.url());
      return url.pathname.endsWith("/metrics") && url.searchParams.get("scope") === "revision" && url.searchParams.get("role") === "production";
    });
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    await production;
    await page.waitForFunction(() => /all indexed callables/.test(document.querySelector("#metrics-state")?.textContent || ""));
    assert.ok(revisionReads() >= reads + 2, "the refresh reads the heat population and the application population");
    const after = await page.locator("#sequence-start-list button").evaluateAll(nodes => nodes.map(node => node.querySelector(".detail").textContent.split(" · ")[0]));
    assert.ok(after.length > 0 && after.every(path => path.startsWith("src/")), `the newly hidden tooling leaves the list: ${JSON.stringify(after)}`);
  }, new Map([["tools/gen.py", toolingPython]]));
});

// A selectable note (files without metrics, files past the size cap) keeps keyboard focus
// and its selection through a resize re-layout.
test("Architecture keeps a focused, selected note through a resize re-layout", {timeout:240000}, async () => {
  const config = layoutRules.replace("[layers.top]\norder = 1\n", "[layers.top]\norder = 1\ndisplay = \"expanded\"\n");
  await published(config, async ({page}) => {
    await ready(page);
    const note = page.locator('#architecture-diagram [data-trellis-note-id]').filter({hasText:/no metrics/}).first();
    const id = await note.getAttribute("data-trellis-note-id");
    assert.equal(id, JSON.stringify(["top", 0]), "src/lib.rs has no callables: the open top band notes it");
    await note.focus(); await page.keyboard.press("Enter"); await settled(page);
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "top");
    await laidOutAt(page, 1100);
    const state = await page.evaluate(() => ({focus:document.activeElement?.getAttribute("data-trellis-note-id"),
      pressed:[...document.querySelectorAll("#architecture-diagram [data-trellis-note-id]")].filter(node => node.getAttribute("aria-pressed") === "true").map(node => node.getAttribute("data-trellis-note-id"))}));
    assert.deepEqual(state, {focus:id, pressed:[id]}, "focus and selection stay on the note");
  }, layoutFiles);
});
