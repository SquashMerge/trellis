"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, readFileSync, writeFileSync, rmSync} = require("node:fs");
const {spawn, spawnSync} = require("node:child_process");
const {createInterface} = require("node:readline");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");
const {arrowGeometry, classicScrollbars, quiet} = require("./architecture-geometry.cjs");

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

// An authored exception keeps the low → top arrow in the inspector: only unexcepted production
// deviations open the Deviations tree (#845).
const lowTopException = (target = "src/low.rs", destination = "src/top.rs") => `
[[exceptions]]
rule = "$layers"
target = { language = "rust", path = "${target}" }
destination = { language = "rust", path = "${destination}" }
reason = "Reviewed so the arrow keeps its evidence inspector"
`;
async function capture(page, name) {
  const directory = process.env.TRELLIS_BROWSER_SCREENSHOTS;
  if (!directory) return;
  mkdirSync(directory, {recursive:true});
  await page.screenshot({path:join(directory, `${name}.png`), fullPage:true});
}
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published(config, callback, extra = new Map(), base = sources, {git = false} = {}) {
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
    if (git) {
      const initialized = spawnSync("git", ["init", "-q", paths.workspace], {env:paths.env, encoding:"utf8"});
      assert.equal(initialized.status, 0, initialized.stderr);
    }
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

// #845: a deviating arrow opens the Deviations tree on its edge; Back returns to the arrow.
test("Architecture deviating arrows open the Deviations tree on their edge, and Back returns to the arrow", {timeout:240000}, async () => published(rules, async selected => {
  const {page, requests} = selected;
  await ready(page);
  const arrows = [
    [page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation(?: · .*)?$/}), /^low → top · \d+$/, "upward"],
    [page.locator("#architecture-diagram").getByRole("button", {name:/production sites · (?:peer|cross)[^·]*deviation/}).first(), /^⇄ mid → peer · \d+$/, "peer"],
  ];
  for (const [arrow, chip, kind] of arrows) {
    assert.equal(await arrow.count(), 1, `the ${kind} arrow is drawn`);
    await arrow.focus(); await page.keyboard.press("Enter");
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
    assert.equal(await page.locator("#architecture-diagram").isHidden(), true);
    const pressed = await page.locator("#architecture-deviations-edges button[aria-pressed=true]").allTextContents();
    assert.equal(pressed.length, 1, `one chip is pressed for the ${kind} arrow`); assert.match(pressed[0], chip);
    assert.equal(await page.locator("#architecture-deviations-calls").isChecked(), false);
    assert.equal(await page.locator("#architecture-deviations-tree .deviation-row[aria-selected=true]").count(), 0, "no line is selected");
    assert.equal(await page.locator("#architecture-review-panel").isVisible(), false, "no rule-instance review list");
    assert.equal(await page.locator("#architecture-panel .architecture-toolbar").isVisible(), false, "the tree replaces the toolbar");
    if (kind === "upward") await capture(page, "architecture-arrow-opens-deviations");
    await page.locator("#architecture-deviations-back").click();
    await page.locator("#architecture-diagram").waitFor({state:"visible"});
    assert.equal(await arrow.evaluate(node => node === document.activeElement), true, `Back returns focus to the ${kind} arrow`);
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Architecture review");
  }
  assert.equal(requests.some(row => row.url.pathname.endsWith("/architecture/details") && row.url.searchParams.has("sourceLayer")), false, "no per-edge instance list is read");
}));

test("Architecture authored exceptions reach the same caller and preserve the exception on Back", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  await addLineException(page, "low → top", "src/low.rs", 1);
  await page.locator("#architecture-reason").fill("Reviewed boundary for sequence navigation");
  await confirmChange(selected, await previewChange(selected));
  await page.locator("#architecture-deviations-back").click();
  await page.locator("#architecture-authored").click();
  const entry = page.locator("#architecture-exceptions [data-exception-index='0']");
  await entry.getByRole("button", {name:"Inspect boundary calls", exact:true}).click();
  const caller = page.locator("#architecture-inspector-content .architecture-caller").filter({hasText:/\blow\b/}).first();
  await caller.getByRole("button", {name:"Open sequence", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#sequence-diagram [data-boundary-highlight='true']") || document.querySelector("#sequence-boundary-status")?.textContent.includes("Show all steps"));
  const showAll = page.locator("#sequence-boundary-status").getByRole("button", {name:"Show all steps", exact:true});
  if (await showAll.count()) await showAll.click();
  await page.locator("#sequence-diagram [data-boundary-highlight='true']").first().waitFor({state:"visible"});
  await capture(page, "architecture-boundary-exception-sequence");
  await page.goBack();
  await page.locator("#architecture-panel").waitFor({state:"visible"});
  await entry.waitFor({state:"visible"});
  assert.match(await entry.textContent(), /Reviewed boundary for sequence navigation/);
  await page.locator("#architecture-inspector-content .architecture-caller").first().waitFor({state:"visible"});
  const exceptedArrow = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation · exception/});
  await exceptedArrow.focus(); await page.keyboard.press("Enter"); await settled(page);
  const arrowCaller = page.locator("#architecture-inspector-content .architecture-caller").filter({hasText:/\blow\b/}).first();
  const selectedSequence = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().method() === "POST");
  await arrowCaller.getByRole("button", {name:"Open sequence", exact:true}).click();
  await selectedSequence;
  await page.locator("#sequence-panel").waitFor({state:"visible"});
  await page.waitForFunction(() => document.querySelector("#sequence-diagram [data-boundary-highlight='true']") || document.querySelector("#sequence-boundary-status")?.textContent.includes("Show all steps"));
  assert.equal(await page.locator("#view-sequence").getAttribute("aria-selected"), "true");
  assert.equal(await page.locator("#view-architecture").getAttribute("aria-selected"), "false");
  await page.goBack();
  await page.locator("#architecture-panel").waitFor({state:"visible"});
  assert.equal(await exceptedArrow.getAttribute("aria-pressed"), "true", "excepted arrows have the same round trip as deviations");
}));

test("Architecture does not invent a caller for syntax-only non-call boundary sites", {timeout:240000}, async () => {
  const withTypes = new Map(sources);
  withTypes.set("src/top.rs", `${withTypes.get("src/top.rs")}pub struct Marker;\n`);
  withTypes.set("src/low.rs", `${lowSource}pub fn takes_type(value: crate::top::Marker) {}\n`);
  await published(rules + lowTopException(), async ({page}) => {
    await ready(page);
    const edge = page.locator("#architecture-diagram").getByRole("button", {name:/production sites · upward\s+deviation · exception/}).first();
    await edge.focus(); await page.keyboard.press("Enter"); await settled(page);
    const pair = page.locator("#architecture-inspector-content > details").filter({has:page.locator("summary", {hasText:/src\/low\.rs → src\/top\.rs/})}).first();
    await pair.locator(":scope > summary").click();
    const unknown = page.locator("#architecture-inspector-content .architecture-caller-unavailable");
    await unknown.first().waitFor({state:"visible"});
    assert.match(await unknown.allTextContents().then(rows => rows.join(" ")), /syntax.only|not a measured call|no measured call/i);
    assert.equal(await unknown.getByRole("button", {name:"Open sequence", exact:true}).count(), 0);
    assert.equal(await page.locator("#architecture-inspector-content .architecture-caller").filter({hasText:/takes_type/}).count(), 0, "enclosing syntax is not a guessed callable link");
  }, new Map(), withTypes);
});


test("Architecture keeps distinct measured callers with colliding stable lookup keys separate", {timeout:240000}, async () => {
  const files = new Map([
    ["src/lib.rs", "mod low; mod top;\n"],
    ["src/low.rs", "pub struct Store; pub trait First { fn same(); } pub trait Second { fn same(); } impl First for Store { fn same() { crate::top::target(); } } impl Second for Store { fn same() { crate::top::target(); } }\n"],
    ["src/top.rs", "pub fn target() {}\n"],
  ]);
  const config = 'version=1\n[layers.low]\norder=2\npaths=["src/low.rs"]\n[layers.top]\norder=1\npaths=["src/top.rs"]\n' + lowTopException();
  await published(config, async ({page}) => {
    await ready(page);
    const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation/});
    await edge.focus(); await page.keyboard.press("Enter"); await settled(page);
    const callers = page.locator("#architecture-inspector-content .architecture-caller");
    assert.equal(await callers.count(), 2, "a stable lookup key is not an exact callable identity");
    const ids = await callers.evaluateAll(nodes => nodes.map(node => node.dataset.symbolId));
    assert.equal(new Set(ids).size, 2);
    for (const id of ids) {
      const exact = page.locator(`#architecture-inspector-content .architecture-caller[data-symbol-id="${id}"]`);
      assert.match(await exact.textContent(), /1 boundary site/);
      const selected = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === id);
      await exact.getByRole("button", {name:"Open sequence", exact:true}).click();
      const response = await selected;
      assert.equal((await response.json()).seed.id, id);
      await page.locator("#sequence-panel").waitFor({state:"visible"});
      await page.waitForFunction(() => document.querySelector("#sequence-boundary-status")?.textContent.includes("1 boundary call highlighted"));
      assert.doesNotMatch(await page.locator("#sequence-boundary-status").textContent(), /not shown/);
      await page.goBack();
      await page.locator("#architecture-panel").waitFor({state:"visible"});
      await page.locator(`#architecture-inspector-content .architecture-caller[data-symbol-id="${id}"]`).waitFor();
    }
  }, new Map(), files);
});


test("Architecture opens the measured inner boundary call inside an argument", {timeout:240000}, async () => {
  const files = new Map([
    ["src/lib.rs", "mod low; mod top;\n"],
    ["src/low.rs", "pub fn nested() { consume(crate::top::target()); } fn consume(_: ()) {}\n"],
    ["src/top.rs", "pub fn target() {}\n"],
  ]);
  const config = 'version=1\n[layers.low]\norder=2\npaths=["src/low.rs"]\n[layers.top]\norder=1\npaths=["src/top.rs"]\n' + lowTopException();
  await published(config, async ({page}) => {
    await ready(page);
    const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^1 production sites · upward\s+deviation/});
    await edge.focus(); await page.keyboard.press("Enter"); await settled(page);
    const caller = page.locator("#architecture-inspector-content .architecture-caller");
    assert.equal(await caller.count(), 1);
    assert.match(await caller.textContent(), /nested.*1 boundary site/s);
    const selected = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().method() === "POST");
    await caller.getByRole("button", {name:"Open sequence", exact:true}).click();
    await selected;
    await page.locator("#sequence-panel").waitFor({state:"visible"});
    await page.waitForFunction(() => document.querySelector("#sequence-boundary-status")?.textContent.includes("1 boundary call highlighted"));
    const highlighted = page.locator("#sequence-diagram [data-boundary-highlight='true']");
    assert.match(await highlighted.allTextContents().then(rows => rows.join(" ")), /crate::top::target/);
    assert.doesNotMatch(await highlighted.allTextContents().then(rows => rows.join(" ")), /consume/);
  }, new Map(), files);
});

const identityKey = identity => JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
const siteTotal = edges => edges.reduce((sum, edge) => sum + edge.count, 0);
const summaryKey = edge => JSON.stringify([edge.sourceLayer, edge.destinationLayer, edge.role, edge.status, edge.excepted, edge.exceptionReason]);
async function ready(page) {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator("#architecture-diagram svg").first().waitFor({state:"attached"});
}
// #845: an exception is added from one line of the Deviations tree, for that line's file pair.
async function addLineException(page, chip, file, line) {
  if (!await page.locator("#architecture-deviations-tree .deviation-row").first().isVisible().catch(() => false)) {
    await page.locator("#architecture-review").click();
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
  }
  await page.locator("#architecture-deviations-edges button").filter({hasText:chip}).click();
  for (let round = 0; round < 30; round++) {
    const next = page.locator('#architecture-deviations-tree .deviation-row[aria-expanded="false"]').first();
    if (!await next.count()) break;
    await next.click();
  }
  const row = page.locator('#architecture-deviations-tree .deviation-row[data-level="3"]').filter({has:page.locator(".deviation-line", {hasText:new RegExp(`^L${line}$`)})})
    .filter({has:page.locator(`.deviation-actions[aria-label$="in ${file}"]`)}).first();
  await row.click();
  await row.locator(".deviation-actions").click();
  await page.locator('[role=menu][aria-label="Line actions"]').getByRole("menuitem", {name:/^Add exception for this file pair/}).click();
  await page.locator("#architecture-editor").waitFor({state:"visible"});
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
  // #845: the production deviation arrow opens the Deviations tree on its edge, listing the same pair.
  await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
  assert.match((await page.locator("#architecture-deviations-edges button[aria-pressed=true]").allTextContents()).join(), /^low → top · \d+$/);
  assert.match(await page.locator("#architecture-deviations-tree").textContent(), /src\/low\.rs/);
  assert.equal(reverse.sites[0].lines.start > 0, true);
  await page.locator("#architecture-deviations-back").click();
  await ready(page);
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
    const key = row.excepted ? "excepted" : row.status === "deviates" ? (ordered && destination.order < source.order ? `↑ ${row.destinationLayer}` : "⇄") :
      row.status !== "conforms" ? "unknown" : destination.shared ? "→shared" : ordered && destination.order > source.order ? "↓" : "allowed";
    const counts = totals.get(row.sourceLayer) || new Map();
    counts.set(key, (counts.get(key) || 0) + row.referenceCount); totals.set(row.sourceLayer, counts);
  }
  const order = ["⇄", "↓", "→shared", "allowed", "excepted", "unknown"];
  const text = (key, count) => key.startsWith("↑ ") ? `↑${count} ${/^≥?1$/.test(String(count)) ? "reference" : "references"} · ${key.slice(2)}` : `${key}${/^[⇄↓]$/.test(key) ? "" : " "}${count}`;
  // Upward deviations are one chip per destination, each a control that follows its band.
  return new Map([...totals].map(([layer, counts]) => [layer, [...[...counts.keys()].filter(key => key.startsWith("↑ ")).sort(), ...order.filter(key => counts.get(key))].map(key => text(key, counts.get(key)))]));
}
const chipTexts = (page, layer) => page.locator(`#architecture-diagram [data-trellis-layer-id="${layer}"] > text.trellis-architecture-stat, #architecture-diagram [data-trellis-layer-action="${layer}"] text.trellis-architecture-stat`)
  .evaluateAll(nodes => nodes.map(node => [node.textContent, node.getAttribute("data-tone")]).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0));

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
  assert.deepEqual(chips.get("low"), ["↑2 references · top"], "the fixture's low layer has a 2-site upward deviation, named by its destination");
  for (const layer of ["low", "mid", "peer", "shared", "top"]) {
    const drawn = await chipTexts(page, layer);
    const expected = (chips.get(layer) || []).map(text => bound(layer) ? text.replace(/^(\D*)(\d+)/, "$1≥$2") : text).sort();
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
    [`${check.counts.deviates} deviation${check.counts.deviates === 1 ? "" : "s"}`, `${check.unassignedFileCount} unassigned file${check.unassignedFileCount === 1 ? "" : "s"}`, "0 exceptions"]);
  const read = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/architecture/details"));
  await page.locator("#architecture-review").click();
  assert.equal((await (await read).json()).payload.total, check.counts.deviates, "the deviations chip opens the tree on exactly what it counts");
  await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
  await page.locator("#architecture-deviations-back").click();
  await ready(page);
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
  assert.deepEqual((await chipTexts(page, "low")).map(([text]) => text), ["↑≥2 references · top"], "a folded band keeps its count chips");
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
test("Architecture arrow inspector keeps exact evidence for an excepted arrow, and lists all unassigned files", {timeout:240000}, async () => published(rules + lowTopException(), async selected => {
  const {page} = selected;
  await ready(page);
  const dto = (await architecture(selected)).data.payload;
  const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation · exception/});
  // In the summary a skip-level count can sit under a peer count; select it by keyboard.
  await edge.focus(); await page.keyboard.press("Enter"); await settled(page);
  assert.equal(await page.locator("#architecture-deviations").isVisible(), false, "an excepted arrow is not a deviation: it keeps the inspector");
  assert.equal(await page.locator("#architecture-review-panel").isVisible(), false, "and opens no review list");
  assert.equal(await page.locator("#architecture-inspector-content input[type=checkbox]").count(), 0);
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /2 exact production structural reference sites · deviates · excepted/);
  const pair = page.locator("#architecture-inspector-content > details").filter({has:page.locator("summary", {hasText:/src\/low\.rs → src\/top\.rs/})}).first();
  await pair.locator(":scope > summary").click();
  assert.match(await pair.textContent(), /2 sampled sites.*0 omitted sites/is);
  const sample = pair.locator(":scope > details").filter({has:page.locator("summary", {hasText:/production · call/})}).first();
  await sample.locator(":scope > summary").click();
  const sampleText = await sample.textContent();
  assert.match(sampleText, /crate::top::target/);
  assert.match(sampleText, /ownerSyntaxId/);
  await sample.getByRole("button", {name:"Read indexed reference source", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#source .source-line.highlight"));
  assert.match(await page.locator("#source-path").textContent(), /src\/low\.rs.*cached snapshot/is);
  const reference = dto.edges.filter(edge => edge.role === "production").flatMap(edge => edge.pairs).find(value => value.source.path === "src/low.rs" && value.target.path === "src/top.rs");
  assert.equal(await page.locator("#source .source-line.highlight .line-number").first().textContent(), String(reference.sites[0].lines.start));

  await page.locator("#architecture-unassigned").click();
  const unassigned = await page.locator("#architecture-review-list").textContent();
  assert.ok(dto.modules.filter(module => module.layer === null).every(module => unassigned.includes(module.identity.path)));
  assert.ok(unassigned.includes("src/only.rs"), "a no-edge file is still listed for assignment");
  assert.ok(unassigned.includes("src/unassigned.rs"));
  await page.setViewportSize({width:390, height:844});
  await noOverflow(page);
}));

test("Architecture adds file-pair exceptions from tree lines, then edits and removes one with exact bytes", {timeout:240000}, async () => published(`# preserved rules header\n${rules}`, async selected => {
  const {page} = selected;
  await ready(page);
  await addLineException(page, "low → top", "src/low.rs", 1);
  await page.locator("#architecture-reason").fill("   ");
  await page.locator("#architecture-preview").click();
  assert.equal(rulesPosts(selected, "preview").length, 0, "blank reason is rejected before any preview request");
  const reason = '<img src=x onerror="globalThis.ruleReasonExecuted=true"> reviewed a concrete pair';
  await page.locator("#architecture-reason").fill(reason);
  const preview = await previewChange(selected);
  assert.equal(preview.pairs.length, 1);
  const body = rulesPosts(selected, "preview")[0].body;
  assert.equal(body.operations.length, 1);
  assert.equal(body.operations[0].type, "addExceptions");
  assert.equal(body.operations[0].reason, reason);
  assert.deepEqual(body.operations[0].pairs.map(pair => [pair.source.path, pair.destination.path]), [["src/low.rs", "src/top.rs"]], "exactly the line's file pair");
  assert.ok(preview.text.startsWith("# preserved rules header\n"), "unrelated authored text is preserved");
  await page.locator("#architecture-editor").scrollIntoViewIfNeeded();
  await capture(page, "architecture-line-exception-preview-desktop");
  await page.setViewportSize({width:390, height:844});
  await page.locator("#architecture-preview-result").scrollIntoViewIfNeeded();
  await capture(page, "architecture-line-exception-preview-mobile");
  await page.setViewportSize({width:1440, height:1000});
  await confirmChange(selected, preview);
  const applied = rulesPosts(selected, "apply")[0].body;
  assert.deepEqual(applied.operations, body.operations);
  assert.equal(applied.baseHash, body.baseHash);
  assert.equal(applied.proposedHash, preview.proposedHash);
  await addLineException(page, "mid → peer", "src/mid.rs", 1);
  await page.locator("#architecture-reason").fill("Reviewed peer pair");
  await confirmChange(selected, await previewChange(selected));
  for (const [source, destination, why] of [["src/low.rs", "src/top.rs", reason], ["src/mid.rs", "src/peer.rs", "Reviewed peer pair"]]) {
    const instance = rulesCheck(selected).instances.find(row => row.rule === "$layers" && row.target.path === source && row.destination?.path === destination);
    assert.equal(instance.excepted, true);
    assert.equal(instance.status, "deviates", "excepted keeps the underlying verdict");
    assert.equal(instance.exceptionReason, why);
  }
  assert.equal(await page.evaluate(() => globalThis.ruleReasonExecuted), undefined);
  assert.equal(await page.locator("#architecture-review-panel img, #architecture-editor img").count(), 0);

  await page.locator("#architecture-authored").click();
  await page.locator("#architecture-exceptions [data-exception-index]").first().waitFor({state:"visible"});
  assert.ok((await page.locator("#architecture-exceptions").textContent()).includes(reason));
  const exception = page.locator("#architecture-exceptions").getByRole("button", {name:"Edit reason", exact:true}).first();
  await exception.click();
  await page.locator("#architecture-reason").fill("Updated reviewed reason");
  const edited = await previewChange(selected);
  await confirmChange(selected, edited);
  assert.ok(rulesCheck(selected).instances.some(row => row.excepted && row.exceptionReason === "Updated reviewed reason"));
  await page.locator("#architecture-authored").click();
  await page.locator("#architecture-exceptions [data-exception-index]").first().waitFor({state:"visible"});
  await page.locator("#architecture-exceptions").getByRole("button", {name:"Remove", exact:true}).first().click();
  const removed = await previewChange(selected);
  await confirmChange(selected, removed);
  assert.equal(rulesCheck(selected).instances.filter(row => row.rule === "$layers" && row.excepted).length, 1, "removal changes only the chosen entry");
}));



test("Architecture preserves a conflicted draft, reloads explicitly and requires a new preview", {timeout:240000}, async () => published(rules, async selected => {
  const {page, paths} = selected;
  await ready(page);
  await addLineException(page, "low → top", "src/low.rs", 1);
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
    await selected.page.waitForFunction(expected => document.querySelector("#architecture-state")?.textContent.includes(expected), state === "no_layers" ? "No authored layers" : "trellis_propose_rules");
    const message = await selected.page.locator("#architecture-panel").textContent();
    assert.match(message, /\.trellis\/rules\.toml/);
    assert.match(message, state === "no_layers" ? /Existing authored exceptions/ : /trellis_propose_rules/);
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
async function mcpRulesCheck(selected) {
  const {paths, pin} = selected;
  const child = spawn(H.binary, ["mcp", "--workspace", paths.workspace], {env:paths.env, stdio:["pipe", "pipe", "pipe"]});
  let stderr = "";
  child.stderr.on("data", chunk => { stderr += chunk; });
  const lines = createInterface({input:child.stdout});
  const replies = lines[Symbol.asyncIterator]();
  const closed = new Promise(resolve => child.once("close", resolve));
  const send = frame => child.stdin.write(`${JSON.stringify(frame)}\n`);
  const reply = async id => {
    for (;;) {
      const next = await replies.next();
      assert.equal(next.done, false, stderr);
      const frame = JSON.parse(next.value);
      if (frame.id === id) { assert.equal(frame.error, undefined, JSON.stringify(frame)); return frame.result; }
    }
  };
  try {
    send({jsonrpc:"2.0",id:1,method:"initialize",params:{protocolVersion:"2025-11-25",capabilities:{},clientInfo:{name:"architecture-count-test",version:"1"}}});
    await reply(1);
    send({jsonrpc:"2.0",method:"notifications/initialized"});
    send({jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"trellis_rules_check",arguments:{schemaVersion:1,expectedBasis:pin}}});
    const result = await reply(2);
    assert.notEqual(result.isError, true, JSON.stringify(result));
    const envelope = result.structuredContent;
    assert.deepEqual(envelope.evidenceBasis, pin);
    return envelope.data;
  } finally {
    lines.close(); child.stdin.end(); child.kill("SIGTERM"); await closed;
  }
}

test("Architecture unassigned files match rules_check without counting external package identities", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  const asset = await page.request.get(`${selected.server.base}/architecture.js`);
  assert.equal(asset.status(), 200);
  assert.equal(await asset.text(), readFileSync(join(__dirname, "../../web/architecture.js"), "utf8"), "the running binary embeds this worktree's Architecture controller");
  const checked = rulesCheck(selected), mcp = await mcpRulesCheck(selected);
  assert.equal(mcp.unassignedFileCount, checked.unassignedFileCount, "the MCP and CLI share the checkout-wide count");
  assert.deepEqual(mcp.rulesFile, checked.rulesFile);
  assert.ok(checked.unassigned.some(identity => identity.kind === "externalPackage"), "the rule catalog includes an unassigned dependency");
  assert.ok(checked.unassigned.length > checked.unassignedFileCount, "identity and file totals genuinely differ");
  for (const role of ["production", "all"]) {
    if (role === "all") await rerendered(page, async () => {
      await H.reveal(page, "#architecture-metric-tests");
      await page.locator("#architecture-metric-tests").check();
    });
    const dto = (await architecture(selected, "max", role)).data.payload;
    assert.equal(dto.indexGeneration, mcp.indexGeneration); assert.equal(dto.indexRevision, mcp.indexRevision);
    assert.deepEqual(dto.rulesFile, mcp.rulesFile);
    const files = dto.modules.filter(module => module.layer === null).map(module => module.identity.path).sort();
    assert.equal(files.length, mcp.unassignedFileCount, "the full native inventory agrees with the checkout-wide rules count");
    assert.ok(files.includes("src/only.rs"), "test-only files still belong to the file inventory");
    assert.ok(files.includes("src/isolated.rs"), "files without references still belong to the file inventory");
    assert.ok(files.includes("src/latin.rs"), "a module-only file without references still belongs to the file inventory");
    assert.ok(dto.partialFiles.some(file => file.path === "src/latin.rs" && file.reason === "not_utf8"));
    const partialModule = dto.modules.find(module => module.identity.path === "src/latin.rs");
    assert.equal(partialModule.declarationCount, 0);
    assert.equal(typeof partialModule.revisionId, "string");
    assert.equal(typeof partialModule.contentHash, "string");
    assert.equal(typeof partialModule.provenanceId, "string");
    assert.ok(checked.unassigned.some(identity => identityKey(identity) === identityKey(partialModule.identity)));
    assert.ok(dto.edges.every(edge => edge.pairs.every(pair => pair.source.path !== "src/latin.rs" && pair.target.path !== "src/latin.rs")), "the partial module invents no reference edges");
    assert.equal(await page.locator("#architecture-unassigned").textContent(), `${checked.unassignedFileCount} unassigned files`);
    await page.locator("#architecture-unassigned").click();
    assert.deepEqual(await page.locator("#architecture-review-list .architecture-rule-row").evaluateAll(rows => rows.map(row => row.dataset.source).sort()), files);
    await page.locator("#architecture-review-close").click();
  }
  assert.ok((await page.locator("#architecture-notice-content").textContent()).includes(`${checked.unassigned.length} unassigned identities (native files and external packages)`));
  await capture(page, "architecture-unassigned-desktop");
  await page.setViewportSize({width:390, height:844});
  await noOverflow(page);
  await capture(page, "architecture-unassigned-mobile");
}, new Map([...libraryFiles, ["src/isolated.rs", "// No references or declarations.\n"], ["src/latin.rs", Buffer.from([0xff])]]), sources, {git:true}));

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

// #525: layout bounds of the drawn diagram, in page pixels. Count tags never sit on text,
// a stat chip, a chip or another tag; #637's arrowGeometry adds bands and arrow segments.
function layoutBounds(geometry) {
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
  return {overlaps, strays, clipped, spills, arrows:geometry, svgWidth:svg.getBoundingClientRect().width, inner, pageFits:document.documentElement.scrollWidth <= innerWidth,
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
function assertLayout(bounds, label) {
  // #637: no arrow segment enters a band or module box; tags sit clear of bands and each other.
  assert.deepEqual(bounds.arrows.crossings, [], `${label}: no arrow crosses a band or a module box`);
  assert.deepEqual(bounds.arrows.tagHits, [], `${label}: no count tag sits on a band, a chip or another tag`);
  assert.deepEqual(bounds.arrows.detached, [], `${label}: every arrow starts on its source band and ends on its target band`);
  assert.deepEqual(bounds.arrows.upward.filter(arrow => arrow.long && !arrow.channel).map(arrow => arrow.name), [], `${label}: long upward arrows use the side channel`);
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
    const bounds = await page.evaluate(layoutBounds, await page.evaluate(arrowGeometry, "#architecture-diagram svg"));
    assertLayout(bounds, `${width}px`);
    assert.ok(bounds.arrows.upward.some(arrow => arrow.long && arrow.channel), `${width}px: low → top runs in the side channel: ${JSON.stringify(bounds.arrows.upward)}`);
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
  // #637: the arrow key's width samples are the drawing's own widths at 1, 10, 100 and 1000 sites
  // (within a pixel: the browser snaps border widths to device pixels).
  const key = await page.evaluate(() => [...document.querySelectorAll(".architecture-key-widths .architecture-key-line")]
    .map(line => [Number(line.dataset.width), parseFloat(getComputedStyle(line).borderTopWidth), window.TrellisArchitecturePele.edgeWidth(Number(line.dataset.width))]));
  assert.equal(key.length, 4);
  for (const [count, drawn, expected] of key) assert.ok(Math.abs(drawn - expected) < 1, `width key at ${count} sites: ${drawn}px, arrows ${expected}px`);
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
  await classicScrollbars(page);
  // An allowed arrow keeps the edge inspector; a deviating one opens the Deviations tree (#845).
  const tag = page.locator("#architecture-diagram [data-trellis-edge-tone=allowed]").first();
  const edgeId = await tag.getAttribute("data-trellis-edge-id");
  await tag.click(); await settled(page);
  const title = await page.locator("#architecture-inspector-title").textContent();
  assert.match(title, / → /);
  const widthOf = () => page.evaluate(() => document.querySelector("#architecture-diagram svg").getBoundingClientRect().width);
  const wide = await widthOf();
  const requests = selected.requests.length;
  await tag.focus();
  await laidOutAt(page, 1200);
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-trellis-edge-id")), edgeId, "keyboard focus returns to the same arrow");
  const narrow = await widthOf();
  assert.ok(narrow < wide, `the drawing narrows with the window (${narrow} < ${wide})`);
  assertLayout(await page.evaluate(layoutBounds, await page.evaluate(arrowGeometry, "#architecture-diagram svg")), "1200px");
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
  assertLayout(await page.evaluate(layoutBounds, await page.evaluate(arrowGeometry, "#architecture-diagram svg")), "inspector hidden");
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

// Deviations tree (#786). A per-test variant of the shared fixture, so the 2-site low → top pins stay intact:
// a type path from low and a hostile-named layer that also deviates.
function deviationsFixture({hostile = false} = {}) {
  const base = new Map(sources);
  base.set("src/top.rs", `${base.get("src/top.rs")}pub struct Marker;\n`);
  base.set("src/low.rs", `${lowSource}pub fn mark(_: crate::top::Marker) {}\n`);
  let config = rules;
  if (hostile) {
    base.set("src/hostile.rs", "pub fn bad() { crate::top::target(); }\n");
    base.set("src/lib.rs", `${base.get("src/lib.rs")}pub mod hostile;\n`);
    config = rules.replace(`[layers.'${hostileLayer}']\nshared = true\npaths = ["src/no-match.rs"]`, `[layers.'${hostileLayer}']\norder = 4\npaths = ["src/hostile.rs"]`);
    assert.notEqual(config, rules);
  }
  return {config, base};
}
const treeRow = (page, level) => page.locator(`#architecture-deviations-tree .deviation-row[data-level="${level}"]`);
const fileRow = (page, path) => page.locator("#architecture-deviations-tree .deviation-row[data-level=\"1\"]").filter({has:page.locator(".deviation-path", {hasText:new RegExp(`^${path.replace(/\./g, "\\.")}$`)})});
async function openDeviations(page) {
  await page.locator("#architecture-review").click();
  await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
}
async function expandTree(page) {
  for (let round = 0; round < 30; round++) {
    const next = page.locator('#architecture-deviations-tree .deviation-row[aria-expanded="false"]').first();
    if (!await next.count()) return;
    await next.click();
  }
  assert.fail("the Deviations tree did not finish expanding");
}
const lineRow = (page, file, start) => page.locator(`#architecture-deviations-tree .deviation-row[data-level="3"]`).filter({has:page.locator(".deviation-line", {hasText:new RegExp(`^L${start}$`)})}).filter({has:page.locator(`.deviation-actions[aria-label$="in ${file}"]`)});
const chipNamed = (page, pattern) => page.locator("#architecture-deviations-edges button").filter({hasText:pattern});
const activeKey = page => page.evaluate(() => document.activeElement?.dataset?.key ?? null);
const menuLabels = page => page.locator('[role=menu][aria-label="Line actions"] [role=menuitem]').allTextContents();

test("Deviations tree classifies lines and opens one menu three ways", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture({hostile:true});
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    await page.locator("#architecture-review").click();
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
    assert.equal(await page.locator("#architecture-diagram").isHidden(), true, "the diagram is hidden, not destroyed");
    assert.match(await page.locator("#architecture-deviations-summary").textContent(), /^\d+ files? · \d+ deviating references? · \d+ calls?$/);
    const badges = await page.locator("#architecture-deviations-tree .deviation-edge").allTextContents();
    assert.ok(badges.length > 0 && badges.every(text => /^[▲⇄] \S+ \d[\d,]*$/.test(text)), `every reach reads "▲ <layer> N": ${JSON.stringify(badges)}`);
    const chips = page.locator("#architecture-deviations-edges button");
    assert.ok(await chips.count() > 1, "edge chips are listed");
    for (const chip of await chips.all()) {
      assert.match(await chip.textContent(), /^(?:⇄ )?.+ · [\d,]+$/, "a chip reads 'from → to · N' (#845)");
      assert.match(await chip.getAttribute("aria-label"), /, [\d,]+ references?$/, "and counts references, not deviations (#816)");
    }
    assert.equal(await page.evaluate(() => globalThis.architectureExecuted), undefined, "a hostile layer name is text");
    assert.equal(await page.locator("#architecture-deviations-edges button").filter({hasText:hostileLayer}).count(), 1, "the hostile layer name renders literally in its chip");
    assert.equal(await page.locator("#architecture-deviations img").count(), 0);
    await chipNamed(page, /low → top/).click();
    await expandTree(page);
    const first = lineRow(page, "src/low.rs", 1);
    assert.equal(await first.locator('.deviation-kind[data-kind="call"]').textContent(), "call");
    const marker = page.locator('#architecture-deviations-tree .deviation-row[data-level="3"] .deviation-kind[data-kind="reference"]');
    assert.ok(await marker.count() >= 1, "the type path in a signature is a reference");
    assert.equal(await marker.first().textContent(), "reference");
    assert.equal(await page.locator("#architecture-deviations-tree .deviation-path").first().evaluate(node => getComputedStyle(node).fontSize), "13px");
    assert.equal(await page.locator("#architecture-deviations .detail").first().evaluate(node => getComputedStyle(node).fontSize), "12px");
    await first.click();
    assert.equal(await first.getAttribute("aria-selected"), "true");
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs:1");
    await capture(page, "architecture-deviations-tree");

    const row = first, button = first.locator(".deviation-actions");
    const triggers = [
      ["the ⋯ button", () => button.click(), () => button],
      ["right-click on the row", () => row.click({button:"right"}), () => row],
      ["Shift+F10 on the focused row", async () => { await row.focus(); await page.keyboard.press("Shift+F10"); }, () => row],
    ];
    let labels = null;
    for (const [name, open, anchor] of triggers) {
      await open();
      const menu = page.locator('[role=menu][aria-label="Line actions"]');
      await menu.waitFor({state:"visible"});
      const now = await menuLabels(page);
      labels ??= now;
      assert.deepEqual(now, labels, name);
      await page.keyboard.press("Escape");
      await menu.waitFor({state:"detached"});
      assert.equal(await anchor().evaluate(node => node === document.activeElement), true, `Escape returns focus to ${name}`);
      assert.equal(await row.getAttribute("aria-expanded"), null, "the line row never carries aria-expanded");
    }
    const order = ["Open in Sequence", "Go to call site", "Copy location", "Add exception for this file pair…"];
    assert.deepEqual(order.filter(label => labels.includes(label)), order, `menu items, in order: ${JSON.stringify(labels)}`);
    assert.deepEqual(labels.filter(label => order.includes(label)), order);
    assert.equal(await page.locator('[role=menu][aria-label="Class actions"]').count(), 0);

    await button.click();
    await page.locator('[role=menu][aria-label="Line actions"]').getByRole("menuitem", {name:"Go to call site"}).click();
    await page.locator("#source-dock").waitFor({state:"visible"});
    await page.locator("#source .source-line.highlight .line-number").first().waitFor();
    assert.equal(await page.locator("#source .source-line.highlight .line-number").first().textContent(), "1");
    assert.equal(await page.evaluate(() => globalThis.architectureExecuted), undefined);
  }, new Map(), base);
});

test("Deviations tree keeps a long function and line on one line each, truncated, with the full text as the tooltip (#911)", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  const method = `enqueue_generated_semantic_import_${"with_a_long_descriptive_suffix_".repeat(4)}end`, callee = `target_${"with_an_equally_long_descriptive_name_".repeat(4)}end`;
  base.set("src/top.rs", `${base.get("src/top.rs")}pub fn ${callee}() {}\n`);
  base.set("src/low.rs", `${base.get("src/low.rs")}pub struct DaemonState;\nimpl DaemonState {\n    pub fn ${method}(&self) { crate::top::${callee}(); }\n}\n`);
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    await openDeviations(page);
    await chipNamed(page, /low → top/).click();
    await expandTree(page);
    const fn = treeRow(page, 2).filter({has:page.locator(".deviation-label", {hasText:`DaemonState::${method}`})});
    assert.equal(await fn.count(), 1, "the function row reads Type::method");
    const label = fn.locator(".deviation-label");
    assert.equal(await label.getAttribute("title"), `DaemonState::${method} · src/low.rs`);
    const line = treeRow(page, 3).filter({has:page.locator(".deviation-symbol", {hasText:callee})});
    assert.equal(await line.count(), 1);
    const symbol = line.locator(".deviation-symbol");
    assert.match(await symbol.getAttribute("title"), new RegExp(`^(?:crate::top::)?${callee} · src/low\\.rs:\\d+$`), "the tooltip holds the full name and the call site");
    for (const [name, row, cell] of [["function", fn, label], ["line", line, symbol]]) {
      const metrics = await cell.evaluate(node => ({lineHeight:parseFloat(getComputedStyle(node).lineHeight), height:node.getBoundingClientRect().height,
        rowHeight:node.closest(".deviation-name").getBoundingClientRect().height, truncated:node.scrollWidth > node.clientWidth, overflow:getComputedStyle(node).textOverflow}));
      assert.ok(metrics.height <= metrics.lineHeight * 1.5 && metrics.rowHeight <= metrics.lineHeight * 1.5, `the ${name} row is one line: ${JSON.stringify(metrics)}`);
      assert.equal(metrics.truncated, true, `the long ${name} name is truncated`);
      assert.equal(metrics.overflow, "ellipsis", `with an ellipsis`);
      assert.equal(await row.locator(".deviation-actions, .deviation-menu").count(), 1);
    }
    await capture(page, "architecture-deviations-long-names");
  }, new Map(), base);
});

test("Deviations tree explains a selection that a filter hides", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    await openDeviations(page);
    const notice = page.locator("#architecture-deviations-notices");
    const hidden = "Selected line is hidden by this filter.";
    const clear = async () => {
      await page.locator("#architecture-deviations-clear").click();
      assert.equal(await page.locator("#architecture-deviations-edges button").first().getAttribute("aria-pressed"), "true", "Clear filter returns to All deviations");
      assert.equal(await page.locator("#architecture-deviations-calls").isChecked(), false);
      assert.equal(await notice.textContent().then(text => text.includes(hidden)), false);
      assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 0, "clearing selects nothing");
    };

    await chipNamed(page, /mid → peer/).click();
    await expandTree(page);
    await treeRow(page, 3).first().click();
    assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 1);
    assert.match(await chipNamed(page, /mid → peer/).first().textContent(), /^⇄ /);
    await chipNamed(page, /low → top/).click();
    assert.match(await notice.textContent(), new RegExp(hidden.replace(".", "\\.")));
    assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 0);
    assert.match(await page.evaluate(() => document.activeElement?.textContent ?? ""), /low → top/, "focus stays on the chip that changed");
    assert.equal(await chipNamed(page, /low → top/).getAttribute("aria-pressed"), "true");
    await clear();

    await expandTree(page);
    const reference = page.locator('#architecture-deviations-tree .deviation-row[data-level="3"]').filter({has:page.locator('.deviation-kind[data-kind="reference"]')}).first();
    await reference.click();
    assert.equal(await reference.getAttribute("aria-selected"), "true");
    await page.locator("#architecture-deviations-calls").check();
    assert.match(await notice.textContent(), new RegExp(hidden.replace(".", "\\.")));
    assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 0);
    assert.equal(await page.evaluate(() => document.activeElement?.id), "architecture-deviations-calls", "focus stays on Calls only");
    await expandTree(page);
    const tags = await page.locator('#architecture-deviations-tree .deviation-row[data-level="3"] .deviation-kind').evaluateAll(nodes => nodes.map(node => node.dataset.kind));
    assert.ok(tags.length > 0 && tags.every(kind => kind === "call"), `Calls only shows exactly the lines tagged call (#816): ${JSON.stringify(tags)}`);
    await clear();
  }, new Map(), base);
});

test("Deviations tree keyboard navigation", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    await openDeviations(page);
    const tree = page.locator("#architecture-deviations-tree");
    const stops = () => tree.locator('.deviation-row[tabindex="0"]').count();
    const focusedLabel = () => page.evaluate(() => document.activeElement?.getAttribute("aria-label") ?? "");
    const level = () => page.evaluate(() => document.activeElement?.dataset?.level ?? null);
    const expanded = () => page.evaluate(() => document.activeElement?.getAttribute("aria-expanded"));

    await page.locator("#architecture-deviations-calls").focus();
    for (let step = 0; step < 6 && !await tree.evaluate(node => node.contains(document.activeElement)); step++) await page.keyboard.press("Tab");
    assert.equal(await stops(), 1, "one tab stop in the tree");
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("role")), "treeitem");
    assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("tabindex")), "0");
    await page.keyboard.press("Tab");
    assert.equal(await tree.evaluate(node => node.contains(document.activeElement)), false, "one more Tab leaves the tree");
    await page.keyboard.press("Shift+Tab");

    const rows = () => tree.locator(".deviation-row").evaluateAll(nodes => nodes.map(node => node.dataset.key));
    await page.keyboard.press("Home");
    assert.equal(await activeKey(page), (await rows())[0]);
    await page.keyboard.press("End");
    const all = await rows();
    assert.equal(await activeKey(page), all.at(-1));
    await page.keyboard.press("ArrowUp");
    assert.equal(await activeKey(page), all.length > 1 ? all.at(-2) : all[0]);
    await page.keyboard.press("Home"); await page.keyboard.press("ArrowDown");
    assert.equal(await activeKey(page), all[1], "ArrowDown moves one visible row");

    const moveTo = async path => {
      await page.keyboard.press("Home");
      for (let step = 0; step < 40 && !(await focusedLabel()).startsWith(`${path},`); step++) await page.keyboard.press("ArrowDown");
      assert.ok((await focusedLabel()).startsWith(`${path},`), `reached ${path}`);
    };
    await moveTo("src/low.rs");
    const fileKey = await activeKey(page);
    assert.equal(await expanded(), "false");
    await page.keyboard.press("ArrowRight");
    assert.equal(await expanded(), "true"); assert.equal(await activeKey(page), fileKey, "ArrowRight expands and keeps focus");
    await page.keyboard.press("ArrowRight");
    assert.equal(await level(), "2", "ArrowRight on an expanded file focuses its first function");
    await page.keyboard.press("ArrowRight");
    assert.equal(await expanded(), "true");
    const fnKey = await activeKey(page);
    await page.keyboard.press("ArrowLeft");
    assert.equal(await expanded(), "false"); assert.equal(await activeKey(page), fnKey, "ArrowLeft collapses and keeps focus");
    await page.keyboard.press("ArrowLeft");
    assert.equal(await activeKey(page), fileKey, "ArrowLeft again focuses the parent file");

    await page.keyboard.press("Enter");
    assert.equal(await expanded(), "false", "Enter toggles a file");
    await page.keyboard.press("Space");
    assert.equal(await expanded(), "true", "Space toggles a file");

    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    assert.equal(await level(), "3", "down to a line");
    await page.keyboard.press("Enter");
    const line = tree.locator(`.deviation-row[data-level="3"][aria-selected="true"]`);
    assert.equal(await line.count(), 1);
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs:1");
    assert.equal(await tree.locator('.deviation-actions[tabindex="0"]').count(), 1);
    assert.equal(await tree.locator('.deviation-actions[tabindex="-1"]').count(), await tree.locator(".deviation-actions").count() - 1);
    assert.equal(await line.locator(".deviation-actions").getAttribute("tabindex"), "0");

    await page.keyboard.press("ArrowLeft");
    assert.equal(await level(), "2");
    await page.keyboard.press("ArrowLeft");
    assert.equal(await expanded(), "false");
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs:1", "collapsing keeps the selection");
    await page.keyboard.press("ArrowRight");
    assert.equal(await tree.locator(`.deviation-row[data-level="3"][aria-selected="true"]`).count(), 1, "re-expanding shows the selected line");
  }, new Map(), base);
});

const highlights = page => page.locator("#sequence-diagram [data-boundary-highlight='true']");
async function showHighlight(page) {
  await page.waitForFunction(() => document.querySelector("#sequence-diagram [data-boundary-highlight='true']") || document.querySelector("#sequence-boundary-status")?.textContent.includes("Show all steps"));
  const status = page.locator("#sequence-boundary-status");
  if (await status.getByRole("button", {name:"Show all steps", exact:true}).count()) await status.getByRole("button", {name:"Show all steps", exact:true}).click();
  await highlights(page).first().waitFor({state:"visible"});
}
const hashCallIds = page => page.evaluate(() => new URLSearchParams(location.hash.slice(location.hash.indexOf("?") + 1)).getAll("callId"));

test("Deviations Open in Sequence keeps its highlight through Back, Forward and reload", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page, paths} = selected;
    await ready(page);
    await openDeviations(page);
    await chipNamed(page, /low → top/).click();
    await expandTree(page);
    const line = lineRow(page, "src/low.rs", 1);
    await line.click();
    await line.locator(".deviation-actions").click();
    await page.locator('[role=menu][aria-label="Line actions"]').getByRole("menuitem", {name:"Open in Sequence"}).click();
    await showHighlight(page);
    assert.equal(await page.locator("#view-sequence").getAttribute("aria-selected"), "true");
    assert.equal(await highlights(page).count(), 1);
    const ids = await hashCallIds(page);
    assert.equal(ids.length, 1, `exactly one callId in ${await page.evaluate(() => location.hash)}`);
    assert.match(ids[0], /^occ:v2:[0-9a-f]{32}$/);
    assert.equal(await highlights(page).first().getAttribute("data-trellis-call-id"), ids[0]);
    assert.match(await page.locator("#sequence-boundary-status").textContent(), /1 boundary call highlighted\./);

    await page.goBack();
    await page.locator("#architecture-deviations-tree").waitFor({state:"visible"});
    const back = lineRow(page, "src/low.rs", 1);
    await back.waitFor({state:"visible"});
    assert.equal(await chipNamed(page, /low → top/).getAttribute("aria-pressed"), "true", "Back keeps the chip");
    assert.equal(await back.getAttribute("aria-selected"), "true", "Back reselects the line");
    assert.equal(await back.evaluate(node => node === document.activeElement), true, "Back focuses the line");
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs:1");

    await page.goForward();
    await showHighlight(page);
    assert.equal(await highlights(page).count(), 1, "Forward keeps the in-memory highlight");

    await page.reload({waitUntil:"domcontentloaded"});
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await showHighlight(page);
    assert.equal(await highlights(page).count(), 1, "a reload rebuilds the highlight from the link");
    assert.deepEqual(await hashCallIds(page), ids);

    await page.goBack();
    await page.goForward();
    await showHighlight(page);
    assert.equal(await highlights(page).count(), 1);
    assert.deepEqual(await hashCallIds(page), ids);

    const stale = await page.evaluate(() => {
      const at = location.hash.indexOf("?"), query = new URLSearchParams(location.hash.slice(at + 1));
      query.set("indexRevision", String(Number(query.get("indexRevision")) + 1));
      return location.hash.slice(0, at + 1) + query;
    });
    await page.evaluate(hash => { location.hash = hash; }, stale);
    await page.reload({waitUntil:"domcontentloaded"});
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    const message = "Cannot open sequence: Sequence link is stale for the selected index snapshot. Reload or select a current link.";
    await page.waitForFunction(text => document.querySelector("#sequence-state")?.textContent === text && document.querySelector("#error")?.textContent === text, message);
    assert.equal(await highlights(page).count(), 0, "a stale link shows no highlight");
  }, new Map(), base);
});

test("Deviations Reload reopens the tree after a rules conflict, with the same chip and no selection", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page, paths} = selected;
    await ready(page);
    await openDeviations(page);
    await chipNamed(page, /low → top/).click();
    await expandTree(page);
    await lineRow(page, "src/low.rs", 1).click();
    await page.locator("#architecture-deviations-calls").check();
    await page.locator("#architecture-deviations-back").click();
    writeFileSync(rulesPath(paths), `${config}\n# an external editor changed the rules at the same revision\n`);
    const reload = page.locator("#architecture-deviations-reload");
    await page.locator("#architecture-review").click();
    await reload.waitFor({state:"visible"});
    assert.match(await page.locator("#architecture-deviations-notices").textContent(), /Rules changed since these deviations were loaded\. Reload deviations\./);
    assert.equal(await page.locator("#architecture-deviations-tree .deviation-row").count(), 0, "no partial tree");
    await reload.click();
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
    assert.equal(await chipNamed(page, /low → top/).getAttribute("aria-pressed"), "true", "Reload keeps the chip");
    assert.equal(await page.locator("#architecture-deviations-calls").isChecked(), true, "Reload keeps Calls only");
    assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 0, "Reload selects nothing");
    assert.equal(await page.locator("#architecture-deviations-reload").count(), 0);
  }, new Map(), base);
});

test("Deviations exception flow previews, confirms and removes the pair from the tree", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page, paths} = selected;
    await ready(page);
    await openDeviations(page);
    await chipNamed(page, /low → top/).click();
    await expandTree(page);
    const line = lineRow(page, "src/low.rs", 1);
    await line.click();
    await line.locator(".deviation-actions").click();
    await page.locator('[role=menu][aria-label="Line actions"]').getByRole("menuitem", {name:/^Add exception for this file pair/}).click();
    await page.locator("#architecture-editor").waitFor({state:"visible"});
    assert.equal(await page.locator("#architecture-edit-scope").textContent(), "Approves all 3 references from src/low.rs to src/top.rs, including future ones on this pair.");
    await page.locator("#architecture-reason").fill("Approved while the tree is open");
    const preview = await previewChange(selected);
    assert.equal(rulesPosts(selected, "apply").length, 0, "preview writes nothing");
    await confirmChange(selected, preview);
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
    assert.equal(await fileRow(page, "src/low.rs").count(), 0, "the excepted pair left the tree");
    assert.equal(await page.locator("#architecture-deviations-tree [aria-selected=true]").count(), 0);
    assert.equal(await page.locator("#architecture-deviations-reload").count(), 0);
  }, new Map(), base);
});

test("Deviations notice counts unknown instances and links to the same-layer and unassigned references", {timeout:240000}, async () => {
  const {config, base} = deviationsFixture();
  await published(config, async selected => {
    const {page} = selected;
    await ready(page);
    await openDeviations(page);
    const notice = page.locator("#architecture-deviations-notices");
    assert.match(await notice.textContent(), /^.*\d+ unknown rule instances? (?:is|are) not deviations\./s);
    await page.locator("#architecture-deviations-notices").getByRole("button", {name:"Inspect same-layer and unassigned references"}).click();
    await page.locator("#architecture-diagram").waitFor({state:"visible"});
    assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Same-layer and unassigned references");
    assert.equal(await page.locator("#architecture-review-panel").isVisible(), false);
  }, new Map(), base);
});

// One app (#789): two npm apps, only `alpha` has an authored layer.
const twoApps = new Map([
  ["a/package.json", '{"name":"alpha"}\n'], ["b/package.json", '{"name":"beta"}\n'],
  ["a/main.js", "export function run() { return 1; }\n"],
  ["b/lib.js", "export function lib() { return 2; }\n"], ["b/extra.js", "export function extra() { return 3; }\n"],
]);
const twoAppRules = `version = 1
[layers.ui]
order = 1
app = "alpha"
paths = ["a/main.js"]
`;
const architectureReads = requests => requests.filter(row => /\/api\/checkouts\/[^/]+\/architecture(?:\/|$)/.test(row.url.pathname)).length;

test("an app with no layers shows the setup help and its unassigned files", {timeout:240000}, async () => published(twoAppRules, async selected => {
  const {page} = selected;
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  const switcher = page.locator("#architecture-apps");
  assert.equal(await switcher.getAttribute("role"), "group"); assert.equal(await switcher.getAttribute("aria-label"), "App");
  assert.deepEqual(await switcher.locator("button").allTextContents(), ["All apps", "alpha · javascript", "beta · javascript"]);
  await switcher.getByRole("button", {name:"alpha · javascript"}).click();
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
  assert.equal(await switcher.getByRole("button", {name:"alpha · javascript"}).getAttribute("aria-pressed"), "true");
  await switcher.getByRole("button", {name:"beta · javascript"}).click();
  const state = page.locator("#architecture-state[data-state=empty]");
  await state.waitFor({state:"visible"});
  const message = await state.textContent();
  assert.match(message, /beta has no layers yet/); assert.match(message, /trellis_propose_rules/); assert.match(message, /app = "beta"/);
  const files = await page.locator("#architecture-diagram .architecture-app-files button").allTextContents();
  assert.deepEqual(files.sort(), ["b/extra.js", "b/lib.js"], "its unassigned files are listed, not dropped");
}, new Map(), twoApps));

test("switching tabs and apps re-draws the held snapshot without reading again", {timeout:240000}, async () => published(twoAppRules, async selected => {
  const {page, requests} = selected;
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
  const before = architectureReads(requests);
  assert.ok(before > 0);
  await page.locator("#view-sequence").click();
  assert.equal(await page.locator("#architecture-panel").isHidden(), true);
  await page.locator("#view-architecture").click();
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
  await page.locator("#architecture-apps").getByRole("button", {name:"alpha · javascript"}).click();
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
  await page.locator("#view-sequence").click(); await page.locator("#view-architecture").click();
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
  assert.equal(await page.locator("#architecture-apps").getByRole("button", {name:"alpha · javascript"}).getAttribute("aria-pressed"), "true", "the scope survives the tab switch");
  assert.equal(architectureReads(requests), before, "no Architecture request after the first read");
}, new Map(), twoApps));

// Badges, stubs and the interface inspector (#789, AC10, AC15, AC25). A Rust server and a JavaScript
// client: `/health` is matched, `/unused` is server-only and `/missing` is client-only.
const httpApps = new Map([
  ["Cargo.toml", '[package]\nname = "srv"\n'],
  ["src/main.rs", 'async fn health() {}\nasync fn unused() {}\nfn router() -> Router {\n    Router::new().route("/health", get(health)).route("/unused", get(unused))\n}\nfn main() {}\n'],
  ["web/package.json", '{"name":"web-app"}\n'],
  ["web/app.js", 'async function load(api) {\n  await api("/health");\n  await api("/missing");\n}\n'],
]);
const httpRules = "version=1\n[layers.server]\napp='srv'\norder=1\npaths=['src/**']\n[layers.client]\napp='web-app'\norder=1\npaths=['web/**']\n";

test("a stub selects its interface with a visible Selected word, opens the inspector and keeps both through a resize", {timeout:240000}, async () => published(httpRules, async ({page}) => {
  await ready(page);
  await page.locator("#architecture-apps").getByRole("button", {name:/^web-app/}).click();
  await page.locator('#architecture-diagram [data-trellis-layer-action="client"]').waitFor({state:"attached"});
  const stub = page.getByRole("button", {name:"HTTP interface to srv: 1 route, 1 call site", exact:true});
  assert.equal(await stub.locator("text.trellis-architecture-stat").textContent(), "→ srv · 1 route · 1 call site");
  assert.equal(await stub.getAttribute("aria-pressed"), "false");
  const tag = stub.locator("text.architecture-selected-tag");
  assert.equal(await tag.evaluate(node => getComputedStyle(node).visibility), "hidden", "no word before a selection");
  await stub.focus(); await page.keyboard.press("Enter"); await settled(page);
  assert.equal(await stub.getAttribute("aria-pressed"), "true");
  assert.equal(await tag.textContent(), "Selected");
  assert.equal(await tag.evaluate(node => getComputedStyle(node).visibility), "visible", "the word marks the selection, not only the colour");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "web-app → srv");
  const band = page.locator('#architecture-diagram [data-trellis-layer-id="client"]');
  await page.waitForFunction(() => document.querySelector('#architecture-diagram [data-trellis-layer-id="client"]')?.getAttribute("aria-label")?.includes("client end"));
  assert.match(await band.getAttribute("aria-label"), /client end/, "the selected stub's band says which end it is");
  const content = page.locator("#architecture-inspector-content");
  assert.match(await content.textContent(), /Client end: web-app · client/); assert.match(await content.textContent(), /Server end: srv · server/);
  await content.getByRole("button", {name:"Open endpoints"}).click(); await settled(page);
  assert.match(await content.textContent(), /GET \/health · exact · 1 call site/);
  assert.match(await content.textContent(), /GET \/unused/); assert.match(await content.textContent(), /unresolved method|\/missing/);
  await content.getByRole("button", {name:"Read handler source"}).first().click();
  await laidOutAt(page, 1100);
  const kept = await page.evaluate(() => ({pressed:[...document.querySelectorAll("#architecture-diagram [aria-pressed=true]")].map(node => node.getAttribute("data-trellis-action-id")),
    focus:document.activeElement?.getAttribute("data-trellis-action-id")}));
  assert.equal(kept.pressed.length, 1); assert.match(kept.pressed[0], /^\["interface","web-app","srv","client","client"\]$/);
  await page.locator('#architecture-diagram [data-trellis-layer-id="client"]').click();
  assert.equal(await stub.getAttribute("aria-pressed"), "false", "selecting another node clears the stub");
  await page.waitForFunction(() => !document.querySelector('#architecture-diagram [data-trellis-layer-id="client"]')?.getAttribute("aria-label")?.includes("client end"));
  await page.locator("#architecture-apps").getByRole("button", {name:/^srv/}).click();
  await page.locator('#architecture-diagram [data-trellis-layer-action="server"]').waitFor({state:"attached"});
  assert.equal(await page.getByRole("button", {name:"HTTP interface from web-app: 1 route", exact:true}).locator("text.trellis-architecture-stat").textContent(), "← web-app · 1 route");
}, new Map(), httpApps));

test("a ↑ badge opens the Deviations tree on its edge with the chip pressed and nothing selected", {timeout:240000}, async () => published(rules, async ({page}) => {
  await ready(page);
  const badge = page.getByRole("button", {name:/^(?:at least )?2 upward production references from low to top · deviation · open in Deviations$/});
  assert.equal(await badge.locator("text.trellis-architecture-stat").textContent(), "↑≥2 references · top");
  assert.equal(await badge.evaluate(node => node.previousElementSibling?.getAttribute("data-trellis-layer-id")), "low", "the badge follows its band");
  await badge.focus(); await page.keyboard.press("Enter");
  await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
  const pressed = await page.locator("#architecture-deviations-edges button[aria-pressed=true]").allTextContents();
  assert.equal(pressed.length, 1); assert.match(pressed[0], /^low → top · \d+$/, "the design's chip: edge and reference count");
  assert.equal(await page.locator("#architecture-deviations-calls").isChecked(), false);
  assert.equal(await page.locator("#architecture-deviations-tree .deviation-row[aria-selected=true]").count(), 0, "no line is selected");
}));

// #845 stale results: a tree read still pending when the user opens another edge, or when the
// index moves on, never paints over what is current.
const detailsPattern = "**/api/checkouts/*/architecture/details?*";
function holdDetails() {
  let release, entered;
  const gate = new Promise(resolve => { release = resolve; }), seen = new Promise(resolve => { entered = resolve; });
  return {gate, seen, release:() => release(), entered:() => entered()};
}
test("Deviations: an edge opened while another edge's read is pending shows only the latest edge", {timeout:240000}, async () => published(rules, async ({page}) => {
  await ready(page);
  const hold = holdDetails();
  const handler = async route => { hold.entered(); await hold.gate; await route.continue(); };
  await page.route(detailsPattern, handler);
  try {
    const up = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation(?: · .*)?$/});
    await up.focus(); await page.keyboard.press("Enter");
    await hold.seen;
    assert.match(await page.locator("#architecture-deviations-state").textContent(), /^Reading deviations/);
    await page.locator("#architecture-deviations-back").click();
    const peer = page.locator("#architecture-diagram").getByRole("button", {name:/production sites · (?:peer|cross)[^·]*deviation/}).first();
    await peer.focus(); await page.keyboard.press("Enter");
    hold.release();
    await page.locator("#architecture-deviations-tree .deviation-row").first().waitFor({state:"visible"});
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const pressed = await page.locator("#architecture-deviations-edges button[aria-pressed=true]").allTextContents();
    assert.equal(pressed.length, 1); assert.match(pressed[0], /^⇄ mid → peer · \d+$/, "the later edge wins");
    const files = await page.locator('#architecture-deviations-tree .deviation-row[data-level="1"] .deviation-path').allTextContents();
    assert.deepEqual(files, ["src/mid.rs"], "only the later edge's files are listed");
  } finally { hold.release(); await page.unroute(detailsPattern, handler); }
}));

test("Deviations: a tree read that answers after the index moved paints nothing", {timeout:240000}, async () => published(rules, async ({page, paths, pin, server, requests}) => {
  await ready(page);
  const hold = holdDetails();
  const handler = async route => {
    hold.entered(); await hold.gate;
    await route.fulfill({status:409, contentType:"application/json", body:JSON.stringify({error:{code:"revision_conflict", message:"The index revision changed."}})});
  };
  await page.route(detailsPattern, handler);
  try {
    const up = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites · upward\s+deviation(?: · .*)?$/});
    await up.focus(); await page.keyboard.press("Enter");
    await hold.seen;
    const changed = `${sources.get("src/peer.rs")}pub fn added() {}\n`;
    writeFileSync(join(paths.workspace, "src/peer.rs"), changed);
    const head = await H.indexThroughApi(server, paths.token, pin, "src/peer.rs", changed);
    assert.ok(head.indexRevision > pin.indexRevision);
    hold.release();
    await page.waitForFunction(() => !document.querySelector("#architecture-deviations-state")?.textContent.startsWith("Reading deviations"));
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.equal(await page.locator("#architecture-deviations-tree .deviation-row").count(), 0, "no rows from a read of the old revision");
    await page.waitForFunction(revision => document.querySelector("#status")?.textContent?.includes(`:${revision}`), head.indexRevision);
  } finally { hold.release(); await page.unroute(detailsPattern, handler); }
  // The conflict moved the view to the new head on purpose; the suite-wide check that every
  // Architecture read uses the fixture's first pin applies to the reads before the move.
  for (let i = requests.length - 1; i >= 0; i--) if (requests[i].url.searchParams.get("indexRevision") && requests[i].url.searchParams.get("indexRevision") !== String(pin.indexRevision)) requests.splice(i, 1);
}));

test("returning from the Deviations tree leaves no stub marked Selected", {timeout:240000}, async () => published(httpRules, async ({page}) => {
  await ready(page);
  await page.locator("#architecture-apps").getByRole("button", {name:/^web-app/}).click();
  const stub = page.getByRole("button", {name:"HTTP interface to srv: 1 route, 1 call site", exact:true});
  await stub.waitFor({state:"attached"});
  await stub.focus(); await page.keyboard.press("Enter"); await settled(page);
  await page.waitForFunction(() => document.querySelector('#architecture-diagram [aria-pressed=true] .architecture-selected-tag'));
  await page.locator("#architecture-review").click();
  await page.locator("#architecture-deviations-back").click();
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Architecture review");
  await page.waitForFunction(() => !document.querySelector('#architecture-diagram [aria-pressed=true]'));
  assert.equal(await stub.getAttribute("aria-pressed"), "false");
  assert.equal(await stub.locator("text.architecture-selected-tag").evaluate(node => getComputedStyle(node).visibility), "hidden");
}, new Map(), httpApps));
