"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, readFileSync, rmSync} = require("node:fs");
const {dirname, join, resolve} = require("node:path");
const {createHash} = require("node:crypto");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");
const ROOT = resolve(__dirname, "../..");
const VENDOR = "/vendor/pele-1.10.0-trellis.2.js";
const VENDOR_SHA256 = "bb4220ed66550152469e0e9a3afb6c532d5812ad62c45126234ed2ca42a0ccea";
const hostilePath = 'nested/<img onerror="globalThis.treemapExecuted=true">.js';
const mixedSource = `// blanks and inline tests count toward physical area

pub fn plain(value: i32) -> i32 { value }
pub fn branch(value: i32) -> i32 { if value > 0 { value } else { 0 } }
#[cfg(test)] mod tests {
    fn nested() { if true { if true { assert!(true); } } }
    fn another() { if true { assert!(true); } }
}
#[cfg(feature = "test-support")] pub fn support() {}
`;
const sources = new Map([
  ["src/lib.rs", "pub mod mixed;\npub mod only;\n"],
  ["src/mixed.rs", mixedSource],
  ["src/only.rs", "#![cfg(test)]\nfn only(value: i32) -> i32 { if value > 0 { value } else { 0 } }\n"],
  ["src/empty.rs", ""],
  ["src/comment.rs", "// no measured callables\n\n"],
  ["src/last.rs", "pub fn last() {}"],
  ["src/a/same.rs", "pub fn first() {}\n"],
  ["src/b/same.rs", "pub fn second() { if true {} }\n"],
  ["nested/clean.js", "function clean() { return 1; }\n"],
  ["nested/no-callables.test.js", "// test root without callables\n"],
  ["nested/empty.test.js", ""],
  [hostilePath, "// untrusted file label\nconst value = 1;\n"],
]);
const fileKey = row => JSON.stringify([row.sourceSetId, row.language, row.path]);
const hiddenTest = row => row.fileRole === "test" || (row.roleCounts.production === 0 && row.roleCounts.test > 0);
// Rust str::lines excludes the final terminator and counts an empty file as zero.
const physicalLines = text => text === "" ? 0 : text.split("\n").length - (text.endsWith("\n") ? 1 : 0);
const nextFrame = page => page.evaluate(() => new Promise(resolveFrame => requestAnimationFrame(() => requestAnimationFrame(resolveFrame))));
const metricRequests = selected => selected.requests.filter(row => row.url.pathname.endsWith("/metrics") && row.url.searchParams.get("scope") === "revision");
async function ready(page) {
  await page.locator("#treemap-state[data-state=ready]").waitFor();
  await page.locator("#treemap-diagram svg").waitFor({state:"attached"});
}
async function capture(page, name) {
  if (!process.env.TRELLIS_BROWSER_SCREENSHOTS) return;
  mkdirSync(process.env.TRELLIS_BROWSER_SCREENSHOTS, {recursive:true});
  await page.screenshot({path:join(process.env.TRELLIS_BROWSER_SCREENSHOTS, `${name}.png`), fullPage:true});
}
async function metrics(selected, aggregate = "max", role = "production") {
  const query = new URLSearchParams({...selected.pin, scope:"revision", aggregate, role});
  const response = await H.api(selected.server.base, selected.paths.token, "GET", H.scoped(selected.server.rootKey, `metrics?${query}`));
  assert.equal(response.status, 200, JSON.stringify(response.data));
  return response.data.payload;
}
function rowAt(dto, path) {
  const candidates = dto.files.filter(row => row.path === path);
  assert.equal(candidates.length, 1, `fixture must have an exact identity for ${path}`);
  return candidates[0];
}
// Attribute values contain JSON and repository punctuation; never interpolate them into CSS.
async function selectFile(page, row, {keyboard, list = false} = {}) {
  const region = list ? "#treemap-files" : "#treemap-diagram";
  if (list && await page.locator(".treemap-file-list").getAttribute("open") === null) await page.locator(".treemap-file-list > summary").click();
  const candidates = page.locator(`${region} [data-trellis-file-id]`);
  const keys = await candidates.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")));
  assert.equal(keys.filter(key => key === fileKey(row)).length, 1, `${region}: one complete file identity`);
  const control = candidates.nth(keys.indexOf(fileKey(row)));
  if (keyboard) { await control.focus(); await page.keyboard.press(keyboard); } else await control.click();
  await page.waitForFunction(path => document.querySelector("#treemap-inspector-title")?.textContent === path, row.path);
  return control;
}
async function nativeKeys(page) {
  return page.locator("#treemap-diagram g.pele-leaf[data-trellis-file-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")).sort());
}
async function listKeys(page) {
  return page.locator("#treemap-files [data-trellis-file-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")).sort());
}
async function sourceText(page) {
  return page.locator("#source .source-line").evaluateAll(lines => lines.map(line => [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n"));
}
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());
async function published(callback) {
  const paths = H.fixture("", "placeholder.txt"); rmSync(paths.source);
  for (const [path, text] of sources) {
    const target = join(paths.workspace, path); mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  mkdirSync(join(paths.workspace, ".trellis"));
  writeFileSync(join(paths.workspace, ".trellis/rules.toml"), 'version = 1\n[layers.source]\norder = 1\npaths = ["**"]\n');
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = JSON.parse(indexed.stdout).status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch(); context = await browser.newContext({viewport:{width:1440, height:1000}, hasTouch:true});
    const page = await context.newPage(), errors = [], requests = [];
    await page.addInitScript(() => { window.treemapCspViolations = []; document.addEventListener("securitypolicyviolation", event => window.treemapCspViolations.push(event.violatedDirective)); });
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text()); });
    page.on("request", request => { const url = new URL(request.url()); if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method()}); });
    await H.connect(page, server, paths.token);
    await page.locator("#view-treemap").click(); await ready(page);
    await callback({page, paths, server, pin, requests});
    assert.deepEqual(errors, [], "production assets and repository labels obey strict CSP");
    assert.deepEqual(await page.evaluate(() => window.treemapCspViolations), []);
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "browsing never indexes");
    for (const row of metricRequests({requests})) {
      assert.equal(row.method, "GET"); assert.ok(["production", "all"].includes(row.url.searchParams.get("role")));
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

test("Treemap renders exact pinned Pele, physical area and independent callable/file populations", {timeout:240000}, async () => published(async selected => {
  const {page, server, pin} = selected;
  const asset = await page.request.get(`${server.base}${VENDOR}`), bytes = await asset.body();
  assert.equal(asset.status(), 200); assert.equal(bytes.length, 711887);
  assert.equal(createHash("sha256").update(bytes).digest("hex"), VENDOR_SHA256);
  assert.equal(createHash("sha256").update(readFileSync(join(ROOT, "web/vendor/pele-1.10.0-trellis.2.js"))).digest("hex"), VENDOR_SHA256);
  const scripts = await page.locator("script[src]").evaluateAll(nodes => nodes.map(node => new URL(node.src).pathname));
  assert.equal(scripts.filter(src => src.includes("/vendor/pele-")).length, 1); assert.ok(scripts.includes(VENDOR));
  const shell = await page.request.get(`${server.base}/`);
  const policy = shell.headers()["content-security-policy"];
  assert.ok(policy); assert.doesNotMatch(policy, /unsafe-inline|unsafe-eval/);
  assert.equal(await page.locator("#treemap-color").inputValue(), "cognitive");
  assert.equal(await page.locator("#treemap-aggregate").inputValue(), "max");
  assert.equal(await page.locator("#treemap-metric-tests").isChecked(), false);
  assert.equal(await page.locator("#treemap-hide-tests").isChecked(), true);
  assert.match(await page.locator("#treemap-panel").textContent(), /Area: physical file lines · includes inline test lines/);
  assert.ok(metricRequests(selected).length > 0);
  assert.ok(metricRequests(selected).every(row => row.url.searchParams.get("role") === "production"));
  const dto = await metrics(selected), visible = dto.files.filter(row => !hiddenTest(row));
  assert.equal(dto.files.length, sources.size);
  assert.equal(dto.indexGeneration, pin.indexGeneration); assert.equal(dto.indexRevision, pin.indexRevision);
  for (const row of dto.files) assert.equal(row.physicalLines, physicalLines(sources.get(row.path)), row.path);
  assert.deepEqual(await listKeys(page), visible.map(fileKey).sort());
  assert.deepEqual(await nativeKeys(page), visible.filter(row => row.physicalLines > 0).map(fileKey).sort(), "every positive file has one native Pele leaf; empty files have no invented area");
  assert.ok(await page.locator("#treemap-diagram g.pele-section").count());
  const cuts = visible.map(row => row.metrics.cognitive).filter(value => typeof value === "number" && Number.isFinite(value)).sort((a, b) => a - b);
  const quartiles = [.25, .5, .75].map(p => cuts[Math.min(cuts.length - 1, Math.floor(cuts.length * p))]);
  const colors = ["#343644", "#FFC20A", "#FFAA10", "#FF7A00"];
  const native = await page.locator("#treemap-diagram g.pele-leaf[data-trellis-file-id]").evaluateAll(nodes => nodes.map(node => ({key:node.getAttribute("data-trellis-file-id"), fill:node.querySelector("rect").getAttribute("fill"), label:node.getAttribute("aria-label")})));
  for (const leaf of native) {
    const row = visible.find(candidate => fileKey(candidate) === leaf.key), value = row.metrics.cognitive;
    assert.equal(leaf.fill, value === null ? "#343644" : colors[quartiles.filter(cut => value > cut).length], row.path);
    assert.ok(leaf.label.includes(row.path));
  }
  const legend = await page.locator("#treemap-legend").textContent();
  for (const cut of quartiles) assert.ok(legend.includes(String(cut)), "legend reports real visible cuts");
  const directories = await page.locator("#treemap-diagram [data-trellis-directory-id]").evaluateAll(nodes => nodes.map(node => ({key:JSON.parse(node.getAttribute("data-trellis-directory-id")), label:node.getAttribute("aria-label")})));
  for (const directory of directories) {
    const [sourceSetId, path] = directory.key;
    const total = visible.filter(row => row.sourceSetId === sourceSetId && (!path || row.path.startsWith(path + "/"))).reduce((sum, row) => sum + row.physicalLines, 0);
    assert.ok(directory.label.includes(`${total} physical file lines`), "directory totals count physical file lines once");
  }
  const mixed = rowAt(dto, "src/mixed.rs"), only = rowAt(dto, "src/only.rs"), empty = rowAt(dto, "src/empty.rs"), comment = rowAt(dto, "src/comment.rs");
  assert.ok(mixed.roleCounts.production > 0 && mixed.roleCounts.test > 0);
  assert.equal(only.roleCounts.production, 0); assert.ok(only.roleCounts.test > 0); assert.equal(only.metrics.cognitive, null);
  for (const path of ["nested/empty.test.js", "nested/no-callables.test.js"]) {
    assert.equal(rowAt(dto, path).fileRole, "test"); assert.deepEqual(rowAt(dto, path).roleCounts, {production:0, test:0});
  }
  await selectFile(page, mixed);
  const detail = await page.locator("#treemap-inspector-content").textContent();
  assert.match(detail, /mixed/i); assert.match(detail, /physical file lines/i);
  assert.ok(detail.includes(String(mixed.physicalLines)));
  for (const value of [mixed.sourceSetId, mixed.language, mixed.path]) assert.ok(detail.includes(value));
  const normalizedDetail = detail.toLowerCase().replace(/[^a-z]/g, "");
  for (const label of ["cognitive", "cyclomatic", "maxnesting", "maintainability", "halstead", "fanout"]) assert.ok(normalizedDetail.includes(label));
  await selectFile(page, empty, {list:true, keyboard:"Enter"});
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /0 physical file lines|physical file lines[^\d]*0/i);
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /no.*callables|no measured/i);
  await selectFile(page, comment, {keyboard:"Space"});
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /—/);
  const sourceSet = new Set(visible.map(fileKey)), beforeReads = metricRequests(selected).length;
  await page.locator("#treemap-hide-tests").uncheck(); await ready(page);
  assert.equal(metricRequests(selected).length, beforeReads, "file visibility is local, not a metric-role read");
  assert.equal(await page.locator("#treemap-metric-tests").isChecked(), false);
  assert.deepEqual(await listKeys(page), dto.files.map(fileKey).sort());
  const onlyControl = await selectFile(page, only);
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /—|unavailable/i);
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /test.only|no production callables/i);
  assert.match(await onlyControl.locator("rect").first().getAttribute("stroke-dasharray"), /\d/, "test-only file has a non-color distinction");
  await page.locator("#treemap-hide-tests").check(); await ready(page);
  await page.locator("#treemap-metric-tests").check(); await ready(page);
  assert.equal(await page.locator("#treemap-hide-tests").isChecked(), true);
  assert.deepEqual(new Set(await listKeys(page)), sourceSet, "including tests in heat cannot reveal hidden files");
  const all = await metrics(selected, "max", "all");
  for (const row of all.files) {
    assert.equal(row.physicalLines, rowAt(dto, row.path).physicalLines);
    assert.deepEqual(row.roleCounts, rowAt(dto, row.path).roleCounts);
  }
  assert.ok(rowAt(all, "src/mixed.rs").metrics.cognitive > mixed.metrics.cognitive);
  await selectFile(page, rowAt(all, "src/mixed.rs"));
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /indexed callables|all.*callables/i);
  await page.locator("#treemap-hide-tests").uncheck(); await ready(page);
  await selectFile(page, rowAt(all, "src/only.rs"));
  assert.ok(rowAt(all, "src/only.rs").metrics.cognitive > 0);
  await page.locator("#treemap-color").selectOption("role"); await ready(page);
  assert.match(await page.locator("#treemap-legend").textContent(), /mixed/i);
  assert.match(await page.locator("#treemap-legend").textContent(), /no.*callables|unmeasured/i);
  await page.locator("#treemap-color").selectOption("maintainabilityIndex"); await ready(page);
  assert.match(await page.locator("#treemap-legend").textContent(), /higher is better|invert/i);
  for (const aggregate of ["sum", "mean"]) {
    await page.locator("#treemap-aggregate").selectOption(aggregate); await ready(page);
    assert.equal(metricRequests(selected).at(-1).url.searchParams.get("aggregate"), aggregate);
    assert.equal(metricRequests(selected).at(-1).url.searchParams.get("role"), "all");
  }
  assert.equal(await page.locator("#treemap-diagram img, #treemap-diagram script, #treemap-diagram foreignObject").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.treemapExecuted), undefined);
  const hostile = rowAt(all, hostilePath); await selectFile(page, hostile, {list:true, keyboard:"Enter"});
  assert.equal(await page.locator("#treemap-inspector-title").textContent(), hostilePath);
  await capture(page, "treemap-desktop");
}));

async function drill(page, sourceSetId, path, keyboard = "Enter") {
  const candidates = page.locator("#treemap-diagram [data-trellis-directory-id]");
  const keys = await candidates.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-directory-id")));
  const index = keys.indexOf(JSON.stringify([sourceSetId, path]));
  assert.notEqual(index, -1, `native directory ${path}`);
  const control = candidates.nth(index);
  await control.focus(); await page.keyboard.press(keyboard); await ready(page);
}

test("Treemap native identities drill and return, Architecture selects exactly, source stays pinned and mobile scrolls", {timeout:240000}, async () => published(async selected => {
  const {page, requests} = selected, dto = await metrics(selected);
  const mixed = rowAt(dto, "src/mixed.rs"), first = rowAt(dto, "src/a/same.rs"), second = rowAt(dto, "src/b/same.rs");
  const visible = dto.files.filter(row => !hiddenTest(row));
  assert.notEqual(fileKey(first), fileKey(second));
  const before = requests.filter(row => row.url.pathname.endsWith("/source")).length;
  await selectFile(page, first, {keyboard:"Enter"});
  assert.equal(await page.locator("#treemap-inspector-title").textContent(), first.path);
  await selectFile(page, second, {keyboard:"Space"});
  assert.equal(requests.filter(row => row.url.pathname.endsWith("/source")).length, before, "selection alone never reads source");
  const controls = page.locator("#treemap-diagram [data-trellis-file-id]");
  const keys = await controls.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")));
  const selectedControl = controls.nth(keys.indexOf(fileKey(second)));
  assert.equal(await selectedControl.getAttribute("aria-pressed"), "true");
  const fill = await selectedControl.locator("rect").first().getAttribute("fill");
  const selectionStroke = await selectedControl.locator("rect").first().evaluate(node => getComputedStyle(node).stroke);
  assert.equal(selectionStroke, "rgb(171, 165, 255)", "selection has the design-token lavender outline");
  await selectedControl.tap();
  assert.equal(await selectedControl.locator("rect").first().getAttribute("fill"), fill, "lavender selection preserves metric heat");
  await drill(page, mixed.sourceSetId, "src");
  assert.deepEqual(await listKeys(page), visible.filter(row => row.sourceSetId === mixed.sourceSetId && row.path.startsWith("src/")).map(fileKey).sort());
  await drill(page, first.sourceSetId, "src/a", "Space");
  assert.deepEqual(await listKeys(page), [fileKey(first)]);
  await page.locator("#treemap-breadcrumbs").getByRole("button", {name:"src", exact:true}).click(); await ready(page);
  assert.ok((await listKeys(page)).includes(fileKey(second)));
  await page.locator("#treemap-breadcrumbs").getByRole("button", {name:"All indexed files", exact:true}).click(); await ready(page);
  assert.deepEqual(await listKeys(page), visible.map(fileKey).sort());
  await page.locator("#treemap-path").fill("src/a/"); await ready(page);
  assert.deepEqual(await listKeys(page), [fileKey(first)]);
  await page.locator("#treemap-path").fill("does-not-exist");
  await page.locator("#treemap-state[data-state=empty]").waitFor();
  assert.equal(await page.locator("#treemap-diagram svg").count(), 0);
  assert.match(await page.locator("#treemap-state").textContent(), /No indexed files/);
  await page.locator("#treemap-path").fill(""); await ready(page);
  await page.locator("#treemap-language").selectOption("rust"); await ready(page);
  assert.deepEqual(await listKeys(page), visible.filter(row => row.language === "rust").map(fileKey).sort());
  await page.locator("#treemap-language").selectOption(""); await ready(page);
  await selectFile(page, mixed);
  await page.locator("#treemap-inspector").getByRole("button", {name:"Read indexed file source", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#source-path")?.textContent.includes("src/mixed.rs") && document.querySelector("#source .source-line"));
  assert.equal(await sourceText(page), mixedSource);
  assert.match(await page.locator("#source-path").textContent(), /cached snapshot/);
  const sourceRequest = requests.filter(row => row.url.pathname.endsWith("/source")).at(-1);
  assert.equal(sourceRequest.url.searchParams.get("path"), mixed.path);
  await page.locator("#view-architecture").click();
  await page.locator("#architecture-state[data-state=ready]").waitFor();
  await page.locator("#architecture-diagram").getByRole("button", {name:/^src\/b\/same\.rs(?:\s|$)/}).click();
  await page.locator("#architecture-inspector").getByRole("button", {name:"Open in Treemap", exact:true}).click();
  await ready(page);
  await page.waitForFunction(path => document.querySelector("#treemap-inspector-title")?.textContent === path, second.path);
  assert.equal(await page.locator("#view-treemap").getAttribute("aria-selected"), "true");
  assert.deepEqual(await listKeys(page), [fileKey(second)], "Architecture opens exact identity in parent scope, not duplicate basename");
  const entered = await page.locator("#treemap-inspector-content").textContent();
  for (const value of [second.sourceSetId, second.language, second.path]) assert.ok(entered.includes(value));
  await page.locator("#treemap-breadcrumbs").getByRole("button", {name:"All indexed files", exact:true}).click(); await ready(page);
  await page.setViewportSize({width:390, height:844});
  if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
  const canvas = await page.locator("#treemap-diagram").evaluate(node => ({overflow:getComputedStyle(node).overflowX, scroll:node.scrollWidth, client:node.clientWidth, width:Number(node.querySelector("svg").getAttribute("width"))}));
  assert.equal(canvas.overflow, "auto"); assert.ok(canvas.scroll > canvas.client); assert.ok(canvas.width >= 640, "native layout is not shrunk to mobile viewport");
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await capture(page, "treemap-mobile");
  await selectFile(page, rowAt(dto, "src/empty.rs"), {list:true, keyboard:"Enter"});
  assert.equal(await page.locator("#treemap-inspector").isVisible(), true);
  await capture(page, "treemap-mobile-inspector");
}));

// Gate actual completed network reads, not fabricated DTOs. The browser receives
// the real Response only after the controlling operation has become obsolete.
async function holdFetch(page, match) {
  await page.evaluate(match => {
    const original = window.fetch;
    let release, armed = true;
    const gate = new Promise(resolveGate => { release = resolveGate; });
    window.treemapFetchGate = {original, release, entered:false, delivered:false};
    window.fetch = async (...args) => {
      const response = await original(...args), url = new URL(args[0], location.href);
      if (armed && url.pathname.endsWith(match.suffix) && Object.entries(match.query || {}).every(([key, value]) => url.searchParams.get(key) === value)) {
        armed = false; window.treemapFetchGate.entered = true;
        await gate; window.treemapFetchGate.delivered = true;
      }
      return response;
    };
  }, match);
}
async function releaseFetch(page) {
  await page.evaluate(() => window.treemapFetchGate.release());
  await page.waitForFunction(() => window.treemapFetchGate.delivered);
  await nextFrame(page);
}
async function restoreFetch(page) {
  await page.evaluate(() => { if (!window.treemapFetchGate) return; window.treemapFetchGate.release(); window.fetch = window.treemapFetchGate.original; delete window.treemapFetchGate; });
}

test("Treemap late production fetch cannot replace all-callable heat and late source cannot navigate after controls change", {timeout:240000}, async () => published(async selected => {
  const {page} = selected, dto = await metrics(selected);
  await page.locator("#treemap-metric-tests").check(); await ready(page);
  await holdFetch(page, {suffix:"/metrics", query:{scope:"revision", role:"production"}});
  try {
    await page.locator("#treemap-metric-tests").uncheck();
    await page.waitForFunction(() => window.treemapFetchGate.entered);
    assert.equal(await page.locator("#treemap-diagram svg").count(), 0);
    assert.equal(await page.locator("#treemap-inspector-content").textContent(), "");
    await page.locator("#treemap-metric-tests").check(); await ready(page);
    const latest = await page.locator("#treemap-diagram").innerHTML();
    await releaseFetch(page);
    assert.equal(await page.locator("#treemap-diagram").innerHTML(), latest);
    assert.match(await page.locator("#treemap-state").textContent(), /all indexed callables/);
  } finally { await restoreFetch(page); }
  await holdFetch(page, {suffix:"/metrics", query:{scope:"revision", aggregate:"sum"}});
  try {
    await page.locator("#treemap-aggregate").selectOption("sum");
    await page.waitForFunction(() => window.treemapFetchGate.entered);
    assert.equal(await page.locator("#treemap-diagram svg").count(), 0);
    await page.locator("#treemap-aggregate").selectOption("mean"); await ready(page);
    const latest = await page.locator("#treemap-diagram").innerHTML();
    await releaseFetch(page);
    assert.equal(await page.locator("#treemap-diagram").innerHTML(), latest);
    assert.match(await page.locator("#treemap-state").textContent(), /mean/);
  } finally { await restoreFetch(page); }
  await holdFetch(page, {suffix:"/source", query:{path:"src/last.rs"}});
  try {
    await selectFile(page, rowAt(dto, "src/last.rs"));
    await page.evaluate(() => { window.treemapHeldSource = [...document.querySelectorAll("#treemap-inspector button")].find(button => button.textContent === "Read indexed file source"); });
    await page.locator("#treemap-inspector").getByRole("button", {name:"Read indexed file source", exact:true}).click();
    await page.waitForFunction(() => window.treemapFetchGate.entered);
    await page.locator("#treemap-aggregate").selectOption("max"); await ready(page);
    const before = selected.requests.filter(row => row.url.pathname.endsWith("/source")).length;
    await releaseFetch(page);
    assert.equal(await page.locator("#source .source-line").count(), 0);
    assert.equal(await page.locator("#treemap-inspector-content").textContent(), "");
    await page.evaluate(() => window.treemapHeldSource.click());
    await nextFrame(page);
    assert.equal(selected.requests.filter(row => row.url.pathname.endsWith("/source")).length, before, "detached source action cannot read after aggregation changes");
  } finally { await restoreFetch(page); await page.evaluate(() => { delete window.treemapHeldSource; }); }
}));

test("Treemap real Pele completion loses to a newer local path scope", {timeout:240000}, async () => published(async selected => {
  const {page} = selected, dto = await metrics(selected), expected = rowAt(dto, "src/a/same.rs");
  await page.evaluate(() => {
    // Pele's bundled exports are getter-only. Replace the global facade,
    // not its read-only render export; the original renderer still runs.
    const original = window.pele;
    let release, armed = true;
    const gate = new Promise(resolveGate => { release = resolveGate; });
    window.treemapRenderGate = {original, release, entered:false, delivered:false};
    window.pele = {...original, render:(...args) => {
      const output = original.render(...args);
      if (!armed || !String(args[0]).startsWith("treemap-beta")) return output;
      armed = false; window.treemapRenderGate.entered = true;
      return gate.then(() => { window.treemapRenderGate.delivered = true; return output; });
    }};
  });
  try {
    await page.locator("#treemap-color").selectOption("cyclomatic");
    await page.waitForFunction(() => window.treemapRenderGate.entered);
    await page.locator("#treemap-path").fill("src/a/"); await ready(page);
    assert.deepEqual(await nativeKeys(page), [fileKey(expected)]);
    const latest = await page.locator("#treemap-diagram").innerHTML();
    await page.evaluate(() => window.treemapRenderGate.release());
    await page.waitForFunction(() => window.treemapRenderGate.delivered); await nextFrame(page);
    assert.equal(await page.locator("#treemap-diagram").innerHTML(), latest);
    assert.deepEqual(await listKeys(page), [fileKey(expected)]);
  } finally {
    await page.evaluate(() => { window.treemapRenderGate.release(); window.pele = window.treemapRenderGate.original; delete window.treemapRenderGate; });
  }
}));

for (const transition of ["generation", "revision", "checkout", "session"]) {
  test(`Treemap late published fetch and held native action are inert after ${transition} ownership changes`, {timeout:240000}, async () => published(async selected => {
    const {page} = selected, dto = await metrics(selected);
    await selectFile(page, rowAt(dto, "src/mixed.rs"));
    await page.evaluate(() => {
      window.treemapHeldTile = document.querySelector("#treemap-diagram [data-trellis-file-id]");
      window.treemapOwnership = {pin:{...status.revision}, root:selectedRootKey, session:epoch};
    });
    await holdFetch(page, {suffix:"/metrics", query:{scope:"revision", role:"production"}});
    try {
      await page.locator("#treemap-refresh").click();
      await page.waitForFunction(() => window.treemapFetchGate.entered);
      // These are the app owner's real sampled context values. Do not perform
      // a request at an invented checkout or pin; only release the old read.
      await page.evaluate(transition => {
        if (transition === "generation") status.revision = {...status.revision, indexGeneration:"11111111-1111-4111-8111-111111111111"};
        else if (transition === "revision") status.revision = {...status.revision, indexRevision:status.revision.indexRevision + 1};
        else if (transition === "checkout") selectedRootKey = "0".repeat(64);
        else epoch++;
      }, transition);
      await releaseFetch(page);
      await page.evaluate(() => window.treemapHeldTile.dispatchEvent(new MouseEvent("click", {bubbles:true})));
      await nextFrame(page);
      assert.equal(await page.locator("#treemap-diagram svg").count(), 0);
      assert.equal(await page.locator("#treemap-files [data-trellis-file-id]").count(), 0);
      assert.equal(await page.locator("#treemap-inspector-content").textContent(), "");
    } finally {
      await restoreFetch(page);
      await page.evaluate(() => { status.revision = window.treemapOwnership.pin; selectedRootKey = window.treemapOwnership.root; epoch = window.treemapOwnership.session; delete window.treemapOwnership; delete window.treemapHeldTile; });
    }
  }));
}
