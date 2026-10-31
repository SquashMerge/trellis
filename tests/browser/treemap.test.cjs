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
// Unsupported source types: explorer metadata only, never Treemap rows.
const unsupportedFiles = ["README.md", "LICENSE", "src/schema.json", "src/notes.md"];
const fileKey = row => JSON.stringify([row.sourceSetId, row.language, row.path]);
const hiddenTest = row => row.fileRole === "test" || (row.roleCounts.production === 0 && row.roleCounts.test > 0);
// Rust str::lines excludes the final terminator and counts an empty file as zero.
const physicalLines = text => text === "" ? 0 : text.split("\n").length - (text.endsWith("\n") ? 1 : 0);
const nextFrame = page => page.evaluate(() => new Promise(resolveFrame => requestAnimationFrame(() => requestAnimationFrame(resolveFrame))));
const metricRequests = selected => selected.requests.filter(row => row.url.pathname.endsWith("/metrics") && row.url.searchParams.get("scope") === "revision");
// #459: a render, or a resize the last action caused (the inspector drawer opening or
// closing), can still replace the SVG after the state reads ready. The next rendering
// frame observes such a resize; the panel publishes "settled" once no render or relayout
// is pending, so later actions and reads never meet a replaced or detached SVG.
async function settled(page) {
  await nextFrame(page);
  await page.waitForFunction(() => document.querySelector("#treemap-diagram")?.dataset.layout === "settled");
}
async function ready(page) {
  await nextFrame(page);
  await page.waitForFunction(() => document.querySelector("#treemap-diagram")?.dataset.layout === "settled" &&
    document.querySelector("#treemap-state")?.dataset.state === "ready" && !!document.querySelector("#treemap-diagram svg"));
}
async function capture(page, name) {
  if (!process.env.TRELLIS_BROWSER_SCREENSHOTS) return;
  mkdirSync(process.env.TRELLIS_BROWSER_SCREENSHOTS, {recursive:true});
  await page.screenshot({path:join(process.env.TRELLIS_BROWSER_SCREENSHOTS, `${name}.png`), fullPage:true});
}
async function metrics(selected, aggregate = "max", role = "production") {
  const query = new URLSearchParams({...selected.pin, scope:"revision", aggregate, role, workbench:"true"});
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
  await settled(page);
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
async function published(callback, extra = new Map()) {
  const paths = H.fixture("", "placeholder.txt"); rmSync(paths.source);
  for (const [path, text] of [...sources, ...extra]) {
    const target = join(paths.workspace, path); mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  for (const path of unsupportedFiles) writeFileSync(join(paths.workspace, path), "not indexed source\n");
  mkdirSync(join(paths.workspace, ".trellis"));
  writeFileSync(join(paths.workspace, ".trellis/rules.toml"), 'version = 1\n[layers.source]\norder = 1\npaths = ["**"]\n');
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
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
      assert.equal(row.method, "GET"); assert.equal(row.url.searchParams.get("workbench"), "true"); assert.ok(["production", "all"].includes(row.url.searchParams.get("role")));
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
    assert.equal(leaf.fill, value === null ? "#343644" : colors[[5, 15, 30].filter(cut => value > cut).length], row.path);
    assert.ok(leaf.label.includes(row.path));
  }
  const legend = await page.locator("#treemap-legend").textContent();
  assert.match(legend, /Fixed per-callable bands.*≤5.*>5–15.*>15–30.*>30/);
  assert.equal(await page.locator("#treemap-relative").isChecked(), false);
  const beforeRelative = metricRequests(selected).length;
  await H.reveal(page, "#treemap-relative"); await page.locator("#treemap-relative").check(); await ready(page);
  assert.equal(metricRequests(selected).length, beforeRelative);
  const relativeLegend = await page.locator("#treemap-legend").textContent();
  assert.match(relativeLegend, /Relative to visible set/);
  for (const cut of quartiles) assert.ok(relativeLegend.includes(String(cut)), "explicit Relative reports visible cuts");
  const relativeLeaves = await page.locator("#treemap-diagram g.pele-leaf[data-trellis-file-id]").evaluateAll(nodes => nodes.map(node => ({key:node.getAttribute("data-trellis-file-id"),fill:node.querySelector("rect").getAttribute("fill")})));
  for (const leaf of relativeLeaves) {
    const value = visible.find(row => fileKey(row) === leaf.key).metrics.cognitive;
    assert.equal(leaf.fill, value === null ? "#343644" : colors[quartiles.filter(cut => value > cut).length]);
  }
  await H.reveal(page, "#treemap-relative"); await page.locator("#treemap-relative").uncheck(); await ready(page);
  await page.locator("#treemap-path").fill("src/mixed.rs"); await ready(page);
  const filtered = await page.locator("#treemap-diagram g.pele-leaf[data-trellis-file-id] rect").first().getAttribute("fill");
  assert.equal(filtered, native.find(leaf => JSON.parse(leaf.key)[2] === "src/mixed.rs").fill, "fixed heat survives path filtering");
  await page.locator("#treemap-path").fill(""); await ready(page);
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
  await H.reveal(page, "#treemap-hide-tests"); await page.locator("#treemap-hide-tests").uncheck(); await ready(page);
  assert.equal(metricRequests(selected).length, beforeReads, "file visibility is local, not a metric-role read");
  assert.equal(await page.locator("#treemap-metric-tests").isChecked(), false);
  assert.deepEqual(await listKeys(page), dto.files.map(fileKey).sort());
  const onlyControl = await selectFile(page, only);
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /—|unavailable/i);
  assert.match(await page.locator("#treemap-inspector-content").textContent(), /test.only|no production callables/i);
  assert.match(await onlyControl.locator("rect").first().getAttribute("stroke-dasharray"), /\d/, "test-only file has a non-color distinction");
  await H.reveal(page, "#treemap-hide-tests"); await page.locator("#treemap-hide-tests").check(); await ready(page);
  await H.reveal(page, "#treemap-metric-tests"); await page.locator("#treemap-metric-tests").check(); await ready(page);
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
  await H.reveal(page, "#treemap-hide-tests"); await page.locator("#treemap-hide-tests").uncheck(); await ready(page);
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
    if (aggregate === "sum") {
      assert.match(await page.locator("#treemap-legend").textContent(), /relative totals; no callable risk bands/);
      assert.deepEqual(await page.locator("#treemap-diagram g.pele-leaf rect").evaluateAll(nodes => [...new Set(nodes.map(node => node.getAttribute("fill")))]), ["#343644"]);
    }
  }
  assert.equal(await page.locator("#treemap-diagram img, #treemap-diagram script, #treemap-diagram foreignObject").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.treemapExecuted), undefined);
  const hostile = rowAt(all, hostilePath); await selectFile(page, hostile, {list:true, keyboard:"Enter"});
  assert.equal(await page.locator("#treemap-inspector-title").textContent(), hostilePath);
  await capture(page, "treemap-desktop");
}));

// A drill waits for its own scope's breadcrumb, never a ready state left from before it.
async function drill(page, sourceSetId, path, keyboard = "Enter") {
  await ready(page);
  const candidates = page.locator("#treemap-diagram [data-trellis-directory-id]");
  const keys = await candidates.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-directory-id")));
  const index = keys.indexOf(JSON.stringify([sourceSetId, path]));
  assert.notEqual(index, -1, `native directory ${path}`);
  const control = candidates.nth(index);
  await control.focus(); await page.keyboard.press(keyboard);
  await page.waitForFunction(leaf => document.querySelector("#treemap-breadcrumbs > button:last-child")?.textContent === leaf, path.split("/").at(-1));
  await ready(page);
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
  const mixedTile = await selectFile(page, mixed);
  const treemapHeat = await mixedTile.locator("rect").first().evaluate(node => getComputedStyle(node).fill);
  await page.locator("#view-sequence").click();
  assert.equal(await page.locator("#metric-aggregate").inputValue(), "max");
  assert.equal(await page.locator("#metric-include-tests").isChecked(), false);
  const explorerFile = page.locator('.file-pick[title="src/mixed.rs"]');
  if (!await explorerFile.count()) await page.locator('.directory-pick[title="src/"]').click();
  await page.waitForFunction(value => document.querySelector('.file-pick[title="src/mixed.rs"] .metric-value > span:last-child')?.textContent === String(value), mixed.metrics.cognitive);
  const explorerHeat = await explorerFile.locator(".metric-swatch").evaluate(node => getComputedStyle(node).backgroundColor);
  assert.equal(explorerHeat, treemapHeat, "same pinned production/max file has identical painted heat in Treemap and Sequence's explorer");
  await page.locator("#view-treemap").click(); await ready(page);
  await selectFile(page, mixed);
  await page.locator("#treemap-inspector").getByRole("button", {name:"Read indexed file source", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#source-path")?.textContent.includes("src/mixed.rs") && document.querySelector("#source .source-line"));
  assert.equal(await sourceText(page), mixedSource);
  assert.match(await page.locator("#source-path").textContent(), /cached snapshot/);
  const sourceRequest = requests.filter(row => row.url.pathname.endsWith("/source")).at(-1);
  assert.equal(sourceRequest.url.searchParams.get("path"), mixed.path);
  await page.locator("#view-architecture").click();
  await page.locator("#architecture-state[data-state=ready]").waitFor();
  // Summary-first Architecture (#391): the full stack is drawn (#449); open the band and folders.
  const opened = async action => {
    await page.evaluate(() => { document.querySelector("#architecture-diagram svg").__old = true; });
    await action();
    await page.waitForFunction(() => { const svg = document.querySelector("#architecture-diagram svg"); return svg && !svg.__old; });
  };
  for (let round = 0; round < 20 && await page.locator('#architecture-diagram [role=button][aria-expanded="false"]').count(); round++) {
    await page.locator('#architecture-diagram [role=button][aria-expanded="false"]').first().focus();
    await opened(() => page.keyboard.press("Enter"));
  }
  const architectureMixed = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/mixed\.rs(?:\s|$)/});
  assert.equal(await page.locator("#architecture-aggregate").inputValue(), "max");
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), false);
  assert.equal(await architectureMixed.locator("rect").first().evaluate(node => getComputedStyle(node).fill), treemapHeat,
    "same pinned production/max file has identical painted heat across Architecture, Treemap and Sequence's explorer");
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
  await H.reveal(page, "#treemap-metric-tests"); await page.locator("#treemap-metric-tests").check(); await ready(page);
  await holdFetch(page, {suffix:"/metrics", query:{scope:"revision", role:"production"}});
  try {
    await H.reveal(page, "#treemap-metric-tests"); await page.locator("#treemap-metric-tests").uncheck();
    await page.waitForFunction(() => window.treemapFetchGate.entered);
    assert.equal(await page.locator("#treemap-diagram svg").count(), 0);
    assert.equal(await page.locator("#treemap-inspector-content").textContent(), "");
    await H.reveal(page, "#treemap-metric-tests"); await page.locator("#treemap-metric-tests").check(); await ready(page);
    const latest = await page.locator("#treemap-diagram").innerHTML();
    await releaseFetch(page);
    assert.equal(await page.locator("#treemap-diagram").innerHTML(), latest);
    assert.match(await page.locator("#treemap-summary").textContent(), /all indexed callables/);
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
    assert.match(await page.locator("#treemap-summary").textContent(), /mean/);
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

async function layout(page) {
  return page.locator("#treemap-diagram").evaluate(node => {
    const style = getComputedStyle(node), svg = node.querySelector("svg");
    return {width:node.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight),
      height:node.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom),
      svgWidth:Number(svg?.getAttribute("width")), svgHeight:Number(svg?.getAttribute("height")),
      overflowX:node.scrollWidth > node.clientWidth, overflowY:node.scrollHeight > node.clientHeight};
  });
}
// The native SVG is the box's content size, floored to whole pixels.
const fills = box => box.width - box.svgWidth >= 0 && box.width - box.svgWidth < 1 && box.height - box.svgHeight >= 0 && box.height - box.svgHeight < 1;
async function filled(page) {
  await page.waitForFunction(() => {
    const node = document.querySelector("#treemap-diagram"), svg = node?.querySelector("svg"), style = node && getComputedStyle(node);
    if (!svg) return false;
    const width = node.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight), height = node.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
    const dx = width - Number(svg.getAttribute("width")), dy = height - Number(svg.getAttribute("height"));
    return dx >= 0 && dx < 1 && dy >= 0 && dy < 1;
  });
  return layout(page);
}

test("Treemap root is the checkout name and fills its panel across resizes; explorer collapses unsupported files and labels its numbers (#425)", {timeout:240000}, async () => published(async selected => {
  const {page} = selected, dto = await metrics(selected);
  const sourceSets = [...new Set(dto.files.map(row => row.sourceSetId))];
  assert.equal(sourceSets.length, 1, "one indexed source set owns the checkout");
  const root = await page.locator("#treemap-diagram g.pele-section[data-trellis-directory-id]").evaluateAll((nodes, key) => nodes
    .filter(node => node.getAttribute("data-trellis-directory-id") === key)
    .map(node => ({label:node.querySelector("text")?.textContent, aria:node.getAttribute("aria-label")})), JSON.stringify([sourceSets[0], ""]));
  assert.equal(root.length, 1);
  assert.match(root[0].label, /^workspace · \d+ lines$/, "root label is the observed checkout directory name");
  assert.ok(root[0].aria.startsWith(`workspace · ${sourceSets[0]} · / · `), "exact source set stays in the accessible label");
  await drill(page, sourceSets[0], "src");
  assert.deepEqual(await page.locator("#treemap-breadcrumbs button").evaluateAll(nodes => nodes.map(node => node.textContent)), ["All indexed files", "workspace", "src"]);
  await page.locator("#treemap-breadcrumbs").getByRole("button", {name:"All indexed files", exact:true}).click(); await ready(page);

  const wide = await filled(page);
  assert.ok(fills(wide), JSON.stringify(wide));
  assert.notEqual(wide.svgWidth, wide.svgHeight, "the layout takes the panel's shape, not a square");
  assert.equal(wide.overflowX || wide.overflowY, false, "a filled layout needs no diagram scroll");
  const mixed = rowAt(dto, "src/mixed.rs"); await selectFile(page, mixed);
  const before = metricRequests(selected).length;
  await page.evaluate(() => {
    const original = window.pele;
    window.treemapRenders = {original, count:0};
    window.pele = {...original, render:(...args) => { if (String(args[0]).startsWith("treemap-beta")) window.treemapRenders.count++; return original.render(...args); }};
  });
  try {
    await page.setViewportSize({width:1180, height:820});
    const narrow = await filled(page);
    assert.ok(fills(narrow) && narrow.svgWidth < wide.svgWidth, JSON.stringify(narrow));
    assert.equal(metricRequests(selected).length, before, "relayout reuses the admitted snapshot");
    assert.equal(await page.locator("#treemap-inspector-title").textContent(), mixed.path, "relayout keeps the selection");
    const tiles = page.locator("#treemap-diagram [data-trellis-file-id]");
    const keys = await tiles.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")));
    assert.equal(await tiles.nth(keys.indexOf(fileKey(mixed))).getAttribute("aria-pressed"), "true");
    // A burst of five box sizes renders once per settled size, not once per change.
    const counted = await page.evaluate(() => window.treemapRenders.count);
    await page.evaluate(async () => {
      const node = document.querySelector("#treemap-diagram");
      for (const height of [520, 540, 560, 580, 600]) { node.style.flex = `0 0 ${height}px`; await new Promise(resolveStep => setTimeout(resolveStep, 20)); }
    });
    const burst = await filled(page);
    assert.ok(fills(burst) && burst.height === 600 - 16, JSON.stringify(burst));
    await page.evaluate(() => new Promise(resolveWait => setTimeout(resolveWait, 400)));
    const renders = await page.evaluate(() => window.treemapRenders.count) - counted;
    assert.ok(renders >= 1 && renders < 5, `bounded relayout: ${renders} renders for 5 resizes`);
    await page.evaluate(() => { document.querySelector("#treemap-diagram").style.flex = ""; });
    assert.ok(fills(await filled(page)));
    // A delivered observer callback whose layout size is unchanged keeps the published SVG's
    // real adapter controls live: click and keyboard still select through its owner gate.
    const kept = await page.evaluate(() => window.treemapRenders.count);
    await page.evaluate(() => new Promise(resolveResize => {
      const node = document.querySelector("#treemap-diagram");
      let calls = 0;
      const observer = new ResizeObserver(() => {
        if (++calls === 1) { node.style.width = `${node.getBoundingClientRect().width - 0.2}px`; return; }
        observer.disconnect(); setTimeout(resolveResize, 400);
      });
      observer.observe(node);
    }));
    assert.equal(await page.evaluate(() => window.treemapRenders.count), kept, "same layout size: no new render");
    await selectFile(page, rowAt(dto, "src/a/same.rs"));
    await selectFile(page, mixed, {keyboard:"Enter"});
    await page.evaluate(() => { document.querySelector("#treemap-diagram").style.width = ""; });
  } finally {
    await page.evaluate(() => { window.pele = window.treemapRenders.original; delete window.treemapRenders; });
  }
  await capture(page, "treemap-fills-panel");

  const tree = page.locator("#file-tree");
  await tree.locator(".directory-pick").first().waitFor();
  const rootMarker = tree.locator(":scope > li.unsupported-files");
  assert.equal(await rootMarker.count(), 1, "one marker for the checkout root");
  assert.equal(await rootMarker.locator("summary").textContent(), "2 files · unsupported source type · not indexed");
  await rootMarker.locator("summary").click();
  assert.deepEqual(await rootMarker.locator("li").evaluateAll(nodes => nodes.map(node => node.textContent).sort()), ["LICENSE", "README.md"]);
  if (!await page.locator('.file-pick[title="src/mixed.rs"]').count()) await page.locator('.directory-pick[title="src/"]').click();
  await page.locator('.file-pick[title="src/mixed.rs"]').waitFor();
  assert.equal(await tree.locator("li.unsupported-files").count(), 2, "one more marker for src/");
  assert.equal(await tree.locator("li.unsupported-files").nth(1).locator("summary").textContent(), "2 files · unsupported source type · not indexed");
  assert.doesNotMatch(await tree.textContent(), /Unsupported source type/, "no per-file unsupported rows");

  // #447: the rail legend is a tooltip on each swatch; the full legend sits in the rail menu.
  assert.equal(await page.locator("#explorer-metric-legend").count(), 0);
  await H.reveal(page, "#metric-legend");
  const legend = page.locator("#metric-legend");
  assert.equal(await legend.isVisible(), true, "the explorer legend is one click away in Treemap");
  assert.match(await legend.textContent(), /Fixed per-callable bands.*≤5.*>5–15.*>15–30.*>30/);
  assert.deepEqual(await legend.locator(".metric-legend-entry .metric-swatch").evaluateAll(nodes => nodes.map(node => getComputedStyle(node).backgroundColor)),
    ["rgb(52, 54, 68)", "rgb(255, 194, 10)", "rgb(255, 170, 16)", "rgb(255, 122, 0)"], "legend uses the shared heat tokens");
  await page.keyboard.press("Escape");
  assert.match(await page.locator('.directory-pick[title="src/"] .metric-value').getAttribute("title"), /^Cognitive · max · production callables · .*Fixed per-callable bands/);
  const folder = await page.locator('.directory-pick[title="src/"]').evaluate(pick => {
    const value = pick.querySelector(".metric-value"), swatch = value?.querySelector(".metric-swatch");
    return {number:value?.querySelector("span:last-child")?.textContent, swatch:swatch?.className, gap:pick.getBoundingClientRect().right - value.getBoundingClientRect().right};
  });
  const value = Number(folder.number);
  assert.ok(Number.isFinite(value), "src/ has a measured folder number");
  assert.equal(folder.swatch, `metric-swatch heat-${[5, 15, 30].filter(cut => value > cut).length}`, "folder swatch uses the legend's fixed band");
  assert.ok(folder.gap >= 0 && folder.gap < 12, "folder number is right-aligned in its row");
  assert.equal(await page.locator("#treemap-legend").getAttribute("role"), "group");
  await capture(page, "treemap-explorer-legend");
  // An open file list scrolls inside its own capped box instead of squeezing the diagram.
  await page.locator(".treemap-file-list > summary").click();
  const list = await page.locator("#treemap-files").evaluate(node => ({max:parseFloat(getComputedStyle(node).maxHeight), height:node.getBoundingClientRect().height, overflow:getComputedStyle(node).overflowY}));
  assert.ok(list.max > 0 && list.height <= list.max + 1 && list.overflow === "auto", JSON.stringify(list));
  assert.ok(fills(await filled(page)), "the diagram still fills its box with the list open");
}));

// #451: 400 one-line files need more than the panel's area at 2500 px² each.
const bulk = new Map(Array.from({length:400}, (_, i) => [`bulk/f${String(i).padStart(3, "0")}.js`, `function f${i}() { return ${i}; }\n`]));
test("Treemap keeps to its panel's width, grows only downward, and keeps its summary behind the view's ⓘ (#451)", {timeout:240000}, async () => published(async selected => {
  const {page} = selected;
  await page.setViewportSize({width:1440, height:900}); await ready(page);
  const box = await layout(page);
  const canvas = await page.locator("#treemap-diagram").evaluate(node => ({scrollWidth:node.scrollWidth, clientWidth:node.clientWidth, scrollHeight:node.scrollHeight, clientHeight:node.clientHeight}));
  assert.ok(box.svgWidth <= box.width && box.width - box.svgWidth < 1, `the layout takes the panel's width: ${JSON.stringify(box)}`);
  assert.ok(canvas.scrollWidth <= canvas.clientWidth, `no sideways scroll: ${JSON.stringify(canvas)}`);
  assert.ok(box.svgHeight > box.height && canvas.scrollHeight > canvas.clientHeight, `a dense layout grows downward only: ${JSON.stringify({box, canvas})}`);
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  const panel = await page.locator("#treemap-panel").evaluate(node => ({sideways:node.scrollWidth > node.clientWidth,
    above:document.querySelector("#treemap-diagram").getBoundingClientRect().top - node.getBoundingClientRect().top,
    controls:node.querySelector(".treemap-controls").getBoundingClientRect().height}));
  assert.equal(panel.sideways, false);
  assert.ok(panel.controls < 70, `one row of controls: ${JSON.stringify(panel)}`);
  assert.ok(panel.above < 200, `controls, breadcrumbs and legend leave the panel to the map: ${JSON.stringify(panel)}`);
  // The ready summary is not a visible line; it sits behind ⓘ, which opens inside the viewport.
  assert.equal(await page.locator("#treemap-state").isVisible(), false);
  assert.equal(await page.locator("#treemap-state").getAttribute("data-state"), "ready");
  assert.equal(await page.locator("#treemap-summary").isVisible(), false);
  await page.locator("#treemap-panel .view-about > summary").click();
  const summary = page.locator("#treemap-summary");
  assert.equal(await summary.isVisible(), true);
  assert.match(await summary.textContent(), /^\d+ of \d+ indexed files · \d+ physical file lines · max · production callables · test files hidden/);
  // #499 places toolbar popovers on toggle, clamped to the panel; wait for that placement.
  await page.waitForFunction(() => document.querySelector("#treemap-panel .view-about .popover-body")?.style.left);
  const card = await page.locator("#treemap-panel .view-about .popover-body").evaluate(node => { const r = node.getBoundingClientRect(); return {left:r.left, right:r.right}; });
  assert.ok(card.left >= 0 && card.right <= 1440, `ⓘ opens inside the viewport: ${JSON.stringify(card)}`);
  await capture(page, "treemap-fit-about");
  await page.keyboard.press("Escape");
  // Loading, empty and error states stay in view, and the summary clears with them.
  await page.locator("#treemap-path").fill("no-matching-files");
  await page.locator("#treemap-state[data-state=empty]").waitFor();
  assert.equal(await page.locator("#treemap-state").isVisible(), true);
  assert.equal(await summary.textContent(), "");
  await page.locator("#treemap-path").fill(""); await ready(page);
  assert.equal(await page.locator("#treemap-state").isVisible(), false);
}, bulk));
