"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync, renameSync} = require("node:fs");
const {join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const hostileName = '<img src=x onerror="globalThis.filenameExecuted=true">.js';
const source = `// UTF-8 before declarations: λ café
class Watcher {
  private int count = 1;
  Watcher(int initial) { count = initial; }
  int measured(int value) {
    if (value > 0) return helper(value);
    return count;
  }
  int helper(int value) { return value + count; }
}
`;
const uses = `class Uses {
  void invoke(Watcher watcher) {
    // measured(999) is not an identifier occurrence.
    String literal = "measured(999)";
    String note = "λ <img src=x onerror='globalThis.snippetExecuted=true'>"; watcher.measured(0); watcher.measured(1);
${Array.from({length:53}, (_, i) => `    watcher.measured(${i + 2});`).join("\n")}
  }
  @Deprecated
  int aVeryLongMeasuredFieldNameThatMustTruncateInsideTheExplorerColumn = 0;
}
`;

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published({git = false, width = 1440, sourceText = source, files = {}} = {}, callback) {
  const paths = H.fixture(sourceText, "Watcher.java");
  for (const [name, text] of Object.entries(files)) writeFileSync(join(paths.workspace, name), text);
  writeFileSync(join(paths.workspace, "Uses.java"), uses);
  writeFileSync(join(paths.workspace, "clean.js"), "const clean = 1;\n");
  writeFileSync(join(paths.workspace, hostileName), "// <script>globalThis.sourceExecuted=true</script>\nconst safe = 'λ';\n");
  let server, browser, context;
  try {
    if (git === "markers") {
      for (const directory of ["changed", "unchanged"]) {
        mkdirSync(join(paths.workspace, directory));
        writeFileSync(join(paths.workspace, directory, "same.js"), "function same() { return 1; }\n");
      }
    }
    if (git) await prepareGit(paths, git !== "markers");
    if (git === "markers") {
      // All mutations precede indexing and watcher startup; no cached-source retention is assumed.
      await dirtyGit(paths);
      writeFileSync(join(paths.workspace, "changed", "same.js"), "function same() { return 2; }\n");
      writeFileSync(join(paths.workspace, hostileName), "const safe = 'changed';\n");
      writeFileSync(join(paths.workspace, "added.js"), "function added() { return 3; }\n");
      const baseline = (await gitCommand(paths, "rev-parse", "HEAD")).stdout.trim();
      const tree = (await gitCommand(paths, "rev-parse", "HEAD^{tree}")).stdout.trim();
      const remote = (await gitCommand(paths, "commit-tree", tree, "-p", baseline, "-m", "Unfetched sibling")).stdout.trim();
      await gitCommand(paths, "update-ref", "refs/remotes/origin/main", remote);
      await gitCommand(paths, "config", "remote.origin.url", "https://example.invalid/never-contact");
      await gitCommand(paths, "config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*");
      const branch = (await gitCommand(paths, "branch", "--show-current")).stdout.trim();
      await gitCommand(paths, "config", `branch.${branch}.remote`, "origin");
      await gitCommand(paths, "config", `branch.${branch}.merge`, "refs/heads/main");
      await gitCommand(paths, "commit", "--allow-empty", "-qm", "Local sibling", "--only");
    }
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width, height:width <= 900 ? 844 : 900}});
    const page = await context.newPage(), requests = [], errors = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text()); });
    page.on("request", request => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method(), body:request.method() === "POST" ? request.postDataJSON() : null});
    });
    await H.connect(page, server, paths.token);
    const selected = {page, paths, server, pin, requests};
    await callback(selected);
    assert.deepEqual(errors, [], "Files uses local assets and safe repository text under the real CSP");
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "browsing never indexes automatically");
    for (const row of requests.filter(row => /\/(source|symbols)$/.test(row.url.pathname))) {
      assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
      assert.ok(row.url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`));
    }
    for (const row of requests.filter(row => /\/(syntax-matches|sequence|navigation)$/.test(row.url.pathname))) {
      assert.deepEqual(row.body.expectedRevision, pin);
      assert.ok(row.url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`));
    }
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
async function read(selected, suffix, body) {
  const {server, paths} = selected;
  const response = await H.api(server.base, paths.token, body === undefined ? "GET" : "POST", H.scoped(server.rootKey, suffix), body);
  assert.equal(response.status, 200, JSON.stringify(response.data));
  return response.data;
}
async function outline(selected, path) {
  const query = new URLSearchParams({...selected.pin, path, offset:0, limit:200});
  const data = await read(selected, `symbols?${query}`);
  assert.deepEqual(data.revision, selected.pin);
  return data;
}
async function cached(selected, path) {
  const query = new URLSearchParams({...selected.pin, path});
  return (await read(selected, `source?${query}`)).file;
}
const sourceReads = requests => requests.filter(row => row.url.pathname.endsWith("/source")).length;
const sequenceReads = requests => requests.filter(row => row.url.pathname.endsWith("/sequence")).length;
async function enterFiles({page, requests}) {
  const before = sourceReads(requests);
  await page.locator("#section-files").focus(); await page.keyboard.press("Enter");
  await page.waitForFunction(() => document.body.classList.contains("files-section"));
  await page.locator("#file-tree .file-pick").first().waitFor({state:"attached"});
  assert.equal(sourceReads(requests), before, "section entry reads metadata, not an implicitly selected source");
}
async function openFile({page}, path) {
  if (await page.locator("#explorer-toggle").isVisible() && await page.locator("#explorer-toggle").getAttribute("aria-expanded") !== "true") await page.locator("#explorer-toggle").click();
  await page.locator("#file-tree .file-pick").filter({hasText:path}).first().click();
  await page.waitForFunction(path => document.querySelector("#source-path")?.textContent.includes(path) && document.querySelector("#source .source-line"), path);
}
// #526: members are rows only under an expanded container; open every container.
async function expandOutline(page) {
  await page.locator(".files-symbol-pick").first().waitFor();
  for (let guard = 0; guard < 50; guard++) {
    const closed = page.locator('.files-symbol-toggle[aria-expanded="false"]');
    if (!await closed.count()) return;
    await closed.first().click();
  }
  assert.fail("outline containers did not stay expanded");
}
async function selectSymbol(page, item) {
  if (await page.locator("#explorer-toggle").isVisible() && await page.locator("#explorer-toggle").getAttribute("aria-expanded") !== "true") await page.locator("#explorer-toggle").click();
  const row = page.locator(`.files-symbol-pick[data-symbol-id="${item.symbol.id}"]`);
  if (!await row.count()) await expandOutline(page);
  await row.waitFor(); await row.focus(); await page.keyboard.press("Enter");
  await page.waitForFunction(name => document.querySelector("#files-symbol-title")?.textContent.includes(name), item.symbol.name);
}
async function assertSource(page, text, range) {
  const lastLine = range && range.endLine > range.startLine && range.endColumn === 1 ? range.endLine - 1 : range?.endLine;
  const highlighted = range ? Array.from({length:lastLine - range.startLine + 1}, (_, i) => String(range.startLine + i)) : null;
  await page.waitForFunction(({text, highlighted}) => {
    const value = [...document.querySelectorAll("#source .source-line")].map(line => [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n");
    const lines = [...document.querySelectorAll("#source .source-line.highlight .line-number")].map(line => line.textContent);
    return value === text && (!highlighted || JSON.stringify(lines) === JSON.stringify(highlighted));
  }, {text, highlighted});
  assert.equal(await page.locator("#source .source-line").evaluateAll(lines => lines.map(line => [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n")), text);
  if (highlighted) assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(), highlighted);
}
async function assertNoOverflow(page) {
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, "only contained source/diagram/diff panes may scroll horizontally");
}
const flatten = steps => steps.flatMap(step => [step, ...flatten(step.children || []), ...flatten(step.alternate || [])]);

test("published Files outlines types and callables with members under an expanded type, UTF-8 syntax locations and mini/full sequence on one pin", {timeout:240000}, async () => published({}, async selected => {
  const {page, pin, requests} = selected;
  await enterFiles(selected);
  await openFile(selected, "Watcher.java");
  const dto = await outline(selected, "Watcher.java"), file = await cached(selected, "Watcher.java");
  assert.equal(file.text, source);
  const kinds = new Set(dto.items.map(item => item.nativeKind));
  for (const kind of ["type", "field", "constructor", "method", "parameter"]) assert.ok(kinds.has(kind), `real Java producer emits ${kind}`);
  const type = dto.items.find(item => item.nativeKind === "type"), method = dto.items.find(item => item.symbol.name === "measured");
  assert.ok(type && method);
  // #526: the collapsed outline is the file's types and callables; fields, constructors,
  // methods and parameters are not top-level rows.
  const rows = () => page.locator(".files-symbol-pick").evaluateAll(nodes => nodes.map(node => node.dataset.symbolId));
  await page.locator(`.files-symbol-pick[data-symbol-id="${type.symbol.id}"]`).waitFor();
  assert.deepEqual(await rows(), [type.symbol.id]);
  const toggle = page.locator(".files-symbol-row").filter({has:page.locator(`.files-symbol-pick[data-symbol-id="${type.symbol.id}"]`)}).locator(".files-symbol-toggle");
  assert.equal(await toggle.getAttribute("aria-expanded"), "false");
  const members = dto.items.filter(item => item.symbol.parent === type.symbol.id && ["field", "constructor", "method"].includes(item.nativeKind));
  assert.equal(await toggle.getAttribute("aria-label"), `Expand Watcher members (${members.length})`);
  await toggle.click();
  assert.equal(await toggle.evaluate(node => document.activeElement === node), true, "focus stays on the toggle");
  assert.equal(await toggle.getAttribute("aria-expanded"), "true");
  assert.deepEqual(await rows(), [type.symbol.id, ...members.map(item => item.symbol.id)], "members follow their type in source order");
  assert.deepEqual(new Set(members.map(item => item.nativeKind)), new Set(["field", "constructor", "method"]));
  for (const item of [type, ...members]) {
    const label = await page.locator(`.files-symbol-pick[data-symbol-id="${item.symbol.id}"]`).textContent();
    assert.ok(label.includes(item.symbol.name)); assert.ok(label.includes(item.nativeKind));
  }
  for (const parameter of dto.items.filter(item => item.nativeKind === "parameter")) {
    assert.equal(await page.locator(`.files-symbol-pick[data-symbol-id="${parameter.symbol.id}"]`).count(), 0, "parameters are never outline rows");
  }
  await toggle.click();
  assert.deepEqual(await rows(), [type.symbol.id], "collapsing hides the members again");
  const before = sequenceReads(requests);
  await selectSymbol(page, type);
  await assertSource(page, source, type.symbol.range);
  assert.equal(sequenceReads(requests), before, "a type is not submitted as a callable sequence");
  assert.equal(await page.locator("#files-open-full").isEnabled(), false);
  await selectSymbol(page, method);
  await assertSource(page, source, method.symbol.range);
  assert.ok(Buffer.byteLength(source.slice(0, source.indexOf("int measured"))) > source.indexOf("int measured"), "fixture actually distinguishes UTF-8 byte and JS character positions");
  assert.match(await page.locator("#files-symbol-return").textContent(), /int/);
  const matches = await read(selected, "syntax-matches", {expectedRevision:pin, symbolId:method.symbol.id, limit:50});
  assert.equal(matches.evidenceKind, "syntaxMatches"); assert.equal(matches.scope.spelling, "measured");
  assert.equal(matches.items.length, 50); assert.ok(matches.continuation);
  await page.waitForFunction(count => document.querySelectorAll("#files-matches .files-match").length === count, matches.items.length);
  // #451: a references list, honestly labelled; a page with more to read is a lower bound.
  assert.equal(await page.locator("#files-matches-title").textContent(), "References to measured · ≥50");
  assert.match(await page.locator("#files-matches-title + .detail").textContent(), /^Syntax matches in the indexed revision · same spelling, not compiler-resolved\./);
  assert.match(await page.locator("#files-inspector").textContent(), /homonym|shadow/i);
  const matchedSources = new Map();
  for (const path of new Set(matches.items.map(match => match.path))) matchedSources.set(path, await cached(selected, path));
  for (const [i, match] of matches.items.entries()) {
    const row = page.locator("#files-matches .files-match").nth(i);
    assert.equal(await row.getAttribute("data-path"), match.path);
    assert.equal(await row.getAttribute("data-start-byte"), String(match.range.startByte));
    assert.equal(await row.getAttribute("data-end-byte"), String(match.range.endByte));
    assert.ok((await row.textContent()).includes(match.snippet));
    const measured = matchedSources.get(match.path);
    assert.equal(Buffer.from(measured.text).subarray(match.range.startByte, match.range.endByte).toString(), "measured");
    assert.ok(![3, 4].includes(match.range.startLine), "comment/string spelling is not a syntax leaf match");
  }
  const remaining = await read(selected, "syntax-matches", {expectedRevision:pin, symbolId:method.symbol.id, limit:50, continuation:matches.continuation});
  assert.equal(remaining.continuation, null); assert.equal(matches.items.length + remaining.items.length, 55);
  await page.locator("#files-matches-more").click();
  await page.waitForFunction(count => document.querySelectorAll("#files-matches .files-match").length === count, 55);
  assert.equal(await page.locator("#files-matches-title").textContent(), "References to measured · 55", "the last page makes the count complete");
  const continuationRequest = requests.filter(row => row.url.pathname.endsWith("/syntax-matches") && row.body.continuation).at(-1);
  assert.deepEqual(continuationRequest.body.continuation, matches.continuation, "UI echoes the exact opaque continuation");
  assert.equal(await page.locator("#files-matches img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.snippetExecuted), undefined);
  const view = await read(selected, "sequence", {seed:method.symbol.id, expectedRevision:pin, showAll:false});
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false);
  const visible = flatten(view.steps).filter(step => !step.hidden);
  assert.deepEqual(await page.locator("#files-mini-sequence [data-trellis-step-id]").evaluateAll(rows => rows.map(row => row.dataset.trellisStepId)), visible.map(step => step.id));
  await page.locator("#files-open-full").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section") && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  assert.deepEqual(await page.locator("#sequence-diagram [data-trellis-step-id]").evaluateAll(rows => rows.map(row => row.dataset.trellisStepId)), visible.map(step => step.id));
  assert.ok(requests.filter(row => row.url.pathname.endsWith("/sequence")).every(row => row.body.seed === method.symbol.id && JSON.stringify(row.body.expectedRevision) === JSON.stringify(pin)));
  await enterFiles(selected);
  const first = matches.items[0];
  await page.locator("#files-matches .files-match").first().click();
  await page.waitForFunction(path => document.querySelector("#source-path")?.textContent.includes(path), first.path);
  await assertSource(page, (await cached(selected, first.path)).text, first.range);
  assert.equal(await page.locator("#files-indexed").getAttribute("aria-pressed"), "true");
  assert.equal(await page.locator("#files-symbol-title").textContent(), "Pinned source location", "opening a match replaces the old declaration inspector");
  assert.equal(await page.locator("#files-symbol-header").textContent(), "");
  assert.equal(await page.locator("#files-open-full").isEnabled(), false);
  await openFile(selected, "Watcher.java"); await selectSymbol(page, method);
  await page.locator("#files-matches .files-match").first().waitFor();
  const last = matches.items.at(-1), row = page.locator("#files-matches .files-match").nth(matches.items.length - 1);
  await row.focus(); await page.keyboard.press("Space");
  await page.waitForFunction(path => document.querySelector("#source-path")?.textContent.includes(path), last.path);
  await assertSource(page, (await cached(selected, last.path)).text, last.range);
  await assertNoOverflow(page);
}));


// #451: Open full lands on Sequence even when Treemap was the last diagram tab, and the
// inspector previews, selects and labels without heavy fills or opaque IDs.
test("Files Open full opens Sequence over a last-used Treemap; the mini fits, the range is quiet and labels carry no IDs", {timeout:240000}, async () => published({}, async selected => {
  const {page, pin, requests} = selected;
  await page.locator("#view-treemap").click();
  await page.waitForFunction(() => document.querySelector("#treemap-state")?.dataset.state === "ready" && document.querySelector("#treemap-diagram svg"));
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const method = (await outline(selected, "Watcher.java")).items.find(item => item.symbol.name === "measured");
  assert.ok(method); await selectSymbol(page, method); await assertSource(page, source, method.symbol.range);

  // Selected range: raised surface with a lavender left bar, readable ink text.
  const tokens = await page.evaluate(() => { const probe = document.createElement("span"); document.body.append(probe);
    const read = (property, value) => { probe.style[property] = value; return getComputedStyle(probe)[property]; };
    const out = {raised:read("backgroundColor", "var(--sm-raised)"), lavender:read("color", "var(--sm-lavender)"), ink:read("color", "var(--ink)")}; probe.remove(); return out; });
  const rows = await page.locator("#source .source-line.highlight").evaluateAll(nodes => nodes.map(node => { const style = getComputedStyle(node); return {background:style.backgroundColor, shadow:style.boxShadow, color:style.color}; }));
  assert.ok(rows.length > 1);
  for (const row of rows) {
    assert.equal(row.background, tokens.raised, JSON.stringify(row)); assert.equal(row.color, tokens.ink);
    assert.ok(row.shadow.includes(tokens.lavender) && /inset/.test(row.shadow), `lavender left bar: ${row.shadow}`);
  }

  // Source-margin actions name the declaration, never its opaque ID.
  const action = page.locator(`.source-diagram-action[data-symbol-id="${method.symbol.id}"]`);
  await action.waitFor({state:"attached"});
  assert.equal(await action.getAttribute("aria-label"), "Sequence for measured · syntax");
  assert.equal(await action.getAttribute("title"), "Sequence · syntax · measured");
  for (const label of await page.locator(".source-diagram-action").evaluateAll(nodes => nodes.flatMap(node => [node.getAttribute("aria-label"), node.title]))) {
    assert.doesNotMatch(label, /sid:|:v1:/, label);
  }

  // The mini-sequence scales to the inspector's width: no sideways scroll.
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false && document.querySelector("#files-mini-sequence svg"));
  const mini = await page.locator("#files-mini-sequence").evaluate(node => {
    const svg = node.querySelector("svg"), box = node.getBoundingClientRect(), drawn = svg.getBoundingClientRect();
    return {scrollWidth:node.scrollWidth, clientWidth:node.clientWidth, left:box.left, right:box.right, svgLeft:drawn.left, svgRight:drawn.right,
      natural:Number(svg.viewBox.baseVal.width), drawn:drawn.width};
  });
  assert.ok(mini.scrollWidth <= mini.clientWidth, `mini fits: ${JSON.stringify(mini)}`);
  assert.ok(mini.svgLeft >= mini.left - 0.5 && mini.svgRight <= mini.right + 0.5, `mini drawn inside its box: ${JSON.stringify(mini)}`);
  assert.ok(mini.drawn <= mini.natural, "the preview scales down, never up past its drawn size");

  // Open full: the last-used Treemap neither shows nor refreshes on the way to Sequence.
  let metricsBefore = requests.filter(row => row.url.pathname.endsWith("/metrics")).length;
  // A Treemap refresh publishes "loading" across its metrics read; record any.
  const watchTreemap = () => page.evaluate(() => {
    window.treemapLoaded = false;
    const state = document.querySelector("#treemap-state");
    window.treemapWatch?.disconnect();
    window.treemapWatch = new MutationObserver(() => { if (state.dataset.state === "loading") window.treemapLoaded = true; });
    window.treemapWatch.observe(state, {attributes:true, attributeFilter:["data-state"]});
  });
  async function onSequence(message) {
    await page.waitForFunction(() => !document.body.classList.contains("files-section") && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
    assert.equal(await page.locator("#view-sequence").getAttribute("aria-selected"), "true", message);
    assert.equal(await page.locator("#view-treemap").getAttribute("aria-selected"), "false", message);
    assert.equal(await page.locator("#sequence-panel").isVisible(), true, message);
    assert.equal(await page.locator("#treemap-panel").isVisible(), false, message);
    assert.equal(await page.evaluate(() => window.treemapLoaded), false, `${message}: Treemap did not refresh`);
    assert.equal(requests.filter(row => row.url.pathname.endsWith("/metrics")).length, metricsBefore, `${message}: no metrics read for the last-used Treemap`);
    assert.notEqual(await page.evaluate(() => document.activeElement?.id), "view-treemap", `${message}: focus is not left on the hidden tab`);
  }
  await watchTreemap();
  await page.locator("#files-open-full").click();
  await onSequence("Open full");
  const route = new URLSearchParams((await page.evaluate(() => location.hash)).slice("#sequence?".length));
  assert.equal(route.get("sid"), method.symbol.id); assert.equal(route.get("indexRevision"), String(pin.indexRevision));
  await assertNoOverflow(page);

  // Back to Files, Treemap becomes the last-used tab again, then Forward to the Sequence entry.
  await page.goBack();
  await page.waitForFunction(() => document.body.classList.contains("files-section") && document.querySelector("#files-symbol-title")?.textContent === "measured");
  await page.locator("#section-diagrams").click();
  await page.locator("#view-treemap").click();
  await page.waitForFunction(() => document.querySelector("#treemap-state")?.dataset.state === "ready" && document.querySelector("#treemap-diagram svg"));
  await page.locator("#section-files").click();
  await page.waitForFunction(() => document.body.classList.contains("files-section"));
  metricsBefore = requests.filter(row => row.url.pathname.endsWith("/metrics")).length;
  await watchTreemap();
  await page.goForward();
  await onSequence("Forward");
}));

// #526 and #514: the preview's caveats sit behind its ⓘ; the drawing scales to the inspector
// and, when its scaled text would be under 9 px, says "Too large to preview · Open full".
test("Files mini-sequence keeps caveats behind ⓘ, fits the inspector and floors unreadable text", {timeout:240000}, async () => published({}, async selected => {
  const {page, pin} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const method = (await outline(selected, "Watcher.java")).items.find(item => item.symbol.name === "measured");
  const view = await read(selected, "sequence", {seed:method.symbol.id, expectedRevision:pin, showAll:false});
  await selectSymbol(page, method);
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false && document.querySelector("#files-mini-sequence")?.dataset.fit);
  // One short status line; the sequence's warnings are notes behind the ⓘ.
  const state = await page.locator("#files-mini-state").textContent();
  assert.match(state, /^Indexed revision [0-9a-f]{8}:\d+ · \d+ incidental steps hidden/);
  for (const warning of view.warnings) assert.ok(!state.includes(warning), `caveat stays out of the status line: ${warning}`);
  const notes = await page.locator("#files-mini-notes li").allTextContents();
  assert.deepEqual(notes.slice(0, view.warnings.length), view.warnings);
  if (notes.length) assert.ok(state.endsWith(`· ${notes.length} ${notes.length === 1 ? "note" : "notes"} in ⓘ`), state);
  assert.equal(await page.locator("#files-mini-notes").isVisible(), false);
  await page.locator("#files-mini-about > summary").click();
  await page.waitForFunction(() => document.querySelector("#files-mini-about .popover-body")?.style.left);
  assert.equal(await page.locator("#files-mini-about .popover-body").isVisible(), true);
  const card = await page.locator("#files-mini-about .popover-body").evaluate(node => { const r = node.getBoundingClientRect(); return {left:r.left, right:r.right, top:r.top}; });
  assert.ok(card.left >= 0 && card.right <= 1440, `ⓘ opens inside the viewport: ${JSON.stringify(card)}`);
  assert.match(await page.locator("#files-mini-about .popover-body").textContent(), /not a runtime trace/);
  await page.keyboard.press("Escape");

  // The fit is measured against the inspector: wide enough draws it, narrow floors it.
  const fit = () => page.locator("#files-mini-sequence").evaluate(node => {
    const svg = node.querySelector("svg"), box = node.getBoundingClientRect(), drawn = svg.getBoundingClientRect();
    return {fit:node.dataset.fit, px:Number(node.dataset.textPx), shown:node.offsetParent !== null, floor:!document.querySelector("#files-mini-floor").hidden,
      scrollWidth:node.scrollWidth, clientWidth:node.clientWidth, left:box.left, right:box.right, svgLeft:drawn.left, svgRight:drawn.right};
  });
  // The strict CSP admits no inline style sheet; the CSSOM sets the frame's columns.
  const widen = width => page.evaluate(width => {
    document.querySelector(".workspace-frame").style.setProperty("grid-template-columns", `200px minmax(0, 1fr) ${width}px`, "important");
  }, width);
  await widen(900);
  await page.waitForFunction(() => document.querySelector("#files-mini-sequence")?.dataset.fit === "scaled", null, {timeout:10000})
    .catch(async error => assert.fail(`${error.message}: ${JSON.stringify(await fit())} ${await page.locator("#files-mini-sequence").evaluate(node => [node.parentElement.clientWidth, node.querySelector("svg")?.getAttribute("viewBox")].join(" "))}`));
  const wide = await fit();
  assert.ok(wide.px >= 9 && wide.shown && !wide.floor, `a wide inspector draws readable text: ${JSON.stringify(wide)}`);
  assert.ok(wide.scrollWidth <= wide.clientWidth, `no sideways scroll: ${JSON.stringify(wide)}`);
  assert.ok(wide.svgLeft >= wide.left - 0.5 && wide.svgRight <= wide.right + 0.5, `the drawing, labels included, stays inside its box: ${JSON.stringify(wide)}`);
  const clipped = await page.locator("#files-mini-sequence svg").evaluate(svg => {
    const view = svg.viewBox.baseVal, box = svg.getBBox();
    return box.x < view.x - 0.5 || box.y < view.y - 0.5 || box.x + box.width > view.x + view.width + 0.5 || box.y + box.height > view.y + view.height + 0.5;
  });
  assert.equal(clipped, false, "no label runs past the drawing's box");
  // A simple return-only callable across ordinary column widths: whenever it is drawn, every label
  // is at least 9 px on screen, measured from geometry (screen box over user box), not the estimate.
  const rendered = () => page.locator("#files-mini-sequence").evaluate(node => {
    const drawnNow = node.offsetParent !== null, svg = node.querySelector("svg");
    if (!drawnNow || !svg) return {drawn:false, floor:!document.querySelector("#files-mini-floor").hidden};
    let smallest = Infinity;
    for (const text of svg.querySelectorAll("text")) {
      if (!text.textContent.trim()) continue;
      const user = text.getBBox(), screen = text.getBoundingClientRect();
      if (!user.height || !screen.height) continue;
      smallest = Math.min(smallest, parseFloat(getComputedStyle(text).fontSize) * screen.height / user.height);
    }
    return {drawn:true, floor:!document.querySelector("#files-mini-floor").hidden, smallest};
  });
  await selectSymbol(page, (await outline(selected, "Watcher.java")).items.find(item => item.symbol.name === "helper"));
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false && document.querySelector("#files-mini-sequence")?.dataset.fit);
  const states = new Set();
  for (const width of [250, 284, 300, 320, 340, 360, 400, 450, 520, 640, 900]) {
    await widen(width);
    await page.waitForFunction(width => Math.abs(document.querySelector("#call-inspector").getBoundingClientRect().width - width) < 1, width);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const state = await rendered();
    states.add(state.drawn);
    if (state.drawn) assert.ok(state.smallest >= 8.95 && !state.floor, `drawn at ${width}px with readable text: ${JSON.stringify(state)}`);
    else assert.ok(state.floor, `not drawn at ${width}px, so the floor shows: ${JSON.stringify(state)}`);
  }
  assert.deepEqual([...states].sort(), [false, true], "the sweep crosses the readable boundary");
  await selectSymbol(page, method);
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false && document.querySelector("#files-mini-sequence")?.dataset.fit);
  await widen(150);
  await page.waitForFunction(() => document.querySelector("#files-mini-sequence")?.dataset.fit === "floor");
  const narrow = await fit();
  assert.ok(narrow.px < 9 && !narrow.shown && narrow.floor, `unreadable text is not drawn: ${JSON.stringify(narrow)}`);
  assert.equal((await page.locator("#files-mini-floor").textContent()).replace(/\s+/g, " ").trim(), "Too large to preview · Open full");
  await page.locator("#files-mini-floor-open").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section") && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  const route = new URLSearchParams((await page.evaluate(() => location.hash)).slice("#sequence?".length));
  assert.equal(route.get("sid"), method.symbol.id);
}));

// #526: a binding whose initializer is a function is a callable row, measured from the real
// JavaScript producer's ranges; a value that merely contains a closure is not.
const bindings = `const handler = (value = loadDefault()) => value;
const result = function () { return 2; }();
const anon = function() { return 2; };
const named = function inner() { return 3; };
const arrow = (value) => value + 1;
const callback = wrap(() => 4);
const plain = 5;
function wrap(f) { return f; }
const matches = function(value) { return /[}]/.test(value); };
const closing = function(value) { return /}/.test(value) ? 1 / 2 : 0; };
const templated = function(name) { return \`}\${name}{\`; };
const ratio = 4 / 2 / 1;
`;
test("Files outlines function-valued bindings and Rust impl targets from the parsed file, not values that contain a closure", {timeout:240000}, async () => published({files:{"bindings.js":bindings, "budget.rs":"pub struct Budget { held: usize }\nimpl Budget {\n    pub fn reserve(&self) -> bool { self.held > 0 }\n}\nimpl std::fmt::Display for crate::other::Remote {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n"}}, async selected => {
  const {page} = selected;
  await enterFiles(selected); await openFile(selected, "bindings.js");
  const dto = await outline(selected, "bindings.js");
  const byName = name => dto.items.find(item => item.symbol.name === name);
  for (const name of ["handler", "result", "anon", "named", "inner", "arrow", "callback", "plain", "wrap", "matches", "closing", "templated", "ratio"]) assert.ok(byName(name), `producer emits ${name}: ${JSON.stringify(dto.items.map(item => [item.symbol.name, item.nativeKind]))}`);
  await page.locator(`.files-symbol-pick[data-symbol-id="${byName("anon").symbol.id}"]`).waitFor();
  const rows = await page.locator(".files-symbol-pick").evaluateAll(nodes => nodes.map(node => node.querySelector(".files-symbol-name").textContent));
  assert.deepEqual(rows, ["handler", "anon", "named", "arrow", "wrap", "matches", "closing", "templated"], "function-valued bindings (regex and template braces included) and declarations; no inner name, invoked function, closure-holding value or plain value");
  // Rust impl blocks: the daemon names the implemented type from the parsed file.
  await openFile(selected, "budget.rs");
  const rust = await outline(selected, "budget.rs");
  const type = rust.items.find(item => item.nativeKind === "type" && item.symbol.name === "Budget");
  assert.deepEqual(rust.items.filter(item => item.nativeKind === "implementation").map(item => item.implTarget), ["Budget", "Remote"]);
  await page.locator(`.files-symbol-pick[data-symbol-id="${type.symbol.id}"]`).waitFor();
  assert.deepEqual(await page.locator(".files-symbol-pick .files-symbol-name").allTextContents(), ["Budget", "Remote"], "no <impl> rows; an impl of a type declared elsewhere is named by that type");
  await page.locator(".files-symbol-row").filter({has:page.locator(`.files-symbol-pick[data-symbol-id="${type.symbol.id}"]`)}).locator(".files-symbol-toggle").click();
  assert.deepEqual(await page.locator(".files-symbol-pick .files-symbol-name").allTextContents(), ["Budget", "held", "reserve", "Remote"]);
}));

// #426: icon plus name on one line, with kind and line right-aligned.
async function assertOneLineRows(page, items) {
  await expandOutline(page);
  const shown = new Set(await page.locator(".files-symbol-pick").evaluateAll(nodes => nodes.map(node => node.dataset.symbolId)));
  items = items.filter(item => shown.has(item.symbol.id));
  assert.ok(items.length > 1, "the expanded outline shows a type and its members");
  for (const item of items) {
    const row = await page.locator(`.files-symbol-pick[data-symbol-id="${item.symbol.id}"]`).evaluate(node => {
      const style = getComputedStyle(node), box = node.getBoundingClientRect(), part = selector => {
        const child = node.querySelector(selector), rect = child?.getBoundingClientRect();
        return child && {text:child.textContent, left:rect.left, right:rect.right, top:rect.top, bottom:rect.bottom, hidden:child.getAttribute("aria-hidden"),
          title:child.title, clipped:child.scrollWidth > child.clientWidth, elements:child.children.length};
      };
      // Geometry, not a row height: the shell's button min-height may exceed one text line.
      return {lineHeight:parseFloat(style.lineHeight), contentRight:box.right - parseFloat(style.paddingRight), label:node.getAttribute("aria-label"), html:node.innerHTML.includes("<img"),
        icon:part(".files-symbol-icon"), name:part(".files-symbol-name"), kind:part(".files-symbol-kind"), line:part(".files-symbol-line")};
    });
    const where = `${item.symbol.name} row ${JSON.stringify(row)}`;
    for (const key of ["icon", "name", "kind", "line"]) assert.ok(row[key].bottom - row[key].top <= row.lineHeight + 1, `${where} ${key} does not wrap`);
    assert.equal(row.icon.hidden, "true", where);
    assert.equal(row.name.text, item.symbol.name); assert.equal(row.name.title, item.symbol.name); assert.equal(row.name.elements, 0);
    assert.equal(row.kind.text, item.nativeKind); assert.equal(row.line.text, `L${item.symbol.range.startLine}`);
    assert.equal(row.label, `${item.symbol.name} · ${item.nativeKind} · L${item.symbol.range.startLine}`, `${where} accessible name contains the visible line label`);
    assert.ok(row.icon.right <= row.name.left && row.name.right <= row.kind.left + 0.5 && row.kind.right <= row.line.left, `${where} reads icon, name, kind, line`);
    assert.ok(Math.abs(row.line.right - row.contentRight) <= 1, `${where} line is right-aligned`);
    for (const key of ["icon", "name", "kind", "line"]) assert.ok(Math.abs(row[key].top - row.name.top) <= 2, `${where} ${key} shares the name's line`);
    assert.equal(row.html, false);
  }
}
const syntaxClasses = page => page.locator("#source .source-line").evaluateAll(lines => lines.map(line => ({
  kind:line.classList.contains("syntax-attribute") ? "attribute" : line.classList.contains("syntax-comment") ? "comment" : "",
  color:getComputedStyle(line).color, nodes:[...line.childNodes].map(node => node.nodeType === Node.TEXT_NODE ? "text" : node.className),
})));

test("Files symbol rows are one line and source lines get subtle text-only syntax colouring", {timeout:240000}, async () => published({}, async selected => {
  const {page} = selected;
  await enterFiles(selected);
  await openFile(selected, "Uses.java");
  const dto = await outline(selected, "Uses.java");
  const long = dto.items.find(item => item.symbol.name.startsWith("aVeryLong"));
  assert.ok(long, "fixture has a long field name");
  await assertOneLineRows(page, dto.items);
  assert.equal(await page.locator(`.files-symbol-pick[data-symbol-id="${long.symbol.id}"] .files-symbol-name`).evaluate(node => node.scrollWidth > node.clientWidth && getComputedStyle(node).textOverflow === "ellipsis"), true, "a long name truncates instead of wrapping");

  const usesText = (await cached(selected, "Uses.java")).text, usesLines = usesText.split("\n");
  const colours = await page.evaluate(() => { const probe = document.createElement("span"); document.body.append(probe);
    const read = value => { probe.style.color = value; return getComputedStyle(probe).color; };
    const out = {gold:read("var(--sm-gold)"), secondary:read("var(--secondary)"), ink:getComputedStyle(document.querySelector("#source")).color}; probe.remove(); return out; });
  let lines = await syntaxClasses(page);
  const annotation = usesLines.findIndex(line => line.trim() === "@Deprecated"), comment = usesLines.findIndex(line => line.trim().startsWith("//"));
  assert.equal(lines[comment].kind, "comment"); assert.equal(lines[comment].color, colours.secondary);
  assert.equal(lines[annotation].kind, "attribute"); assert.equal(lines[annotation].color, colours.gold);
  assert.equal(lines[0].kind, ""); assert.equal(lines[0].color, colours.ink);
  for (const line of lines) assert.deepEqual(line.nodes.filter(node => node === "text" || node === "line-number"), ["line-number", "text"], "colouring never splits or parses repository text");
  await assertSource(page, usesText);
  const selectedRow = page.locator(`.files-symbol-pick[data-symbol-id="${long.symbol.id}"]`);
  await selectSymbol(page, long);
  await assertSource(page, usesText, long.symbol.range);
  assert.equal(await selectedRow.getAttribute("aria-pressed"), "true");
  assert.deepEqual(await selectedRow.evaluate(node => [...node.children].map(child => getComputedStyle(child).color)), Array(4).fill(await selectedRow.evaluate(node => getComputedStyle(node).color)), "selected rows keep ink text on the selection fill");
  // #451: the quiet raised selection keeps the subtle syntax colouring.
  const selectedLines = await syntaxClasses(page);
  assert.equal(selectedLines[annotation].kind, "attribute", "selection keeps the line class");
  const highlightedRows = await page.locator("#source .source-line.highlight").evaluateAll(rows => rows.map(row => ({attribute:row.classList.contains("syntax-attribute"), color:getComputedStyle(row).color})));
  assert.ok(highlightedRows.length);
  for (const row of highlightedRows) assert.equal(row.color, row.attribute ? colours.gold : colours.ink);

  await openFile(selected, hostileName);
  lines = await syntaxClasses(page);
  assert.equal(lines[0].kind, "comment", "a script-looking comment is coloured as text");
  assert.equal(await page.locator("#source script, #source img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.sourceExecuted), undefined);
  await assertSource(page, (await cached(selected, hostileName)).text);

  // Diagrams reuses the cached snapshot without Files colouring.
  await page.locator("#section-diagrams").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section"));
  // Diagrams repaints the cached snapshot after its current view (Architecture on arrival) refreshes.
  await page.waitForFunction(() => document.querySelectorAll("#source .source-line").length > 0 && document.querySelectorAll("#source .syntax-comment, #source .syntax-attribute").length === 0);
}));

test("mini Full from Working tree explains Back to an unsaved blank route and Forward restores its pinned sequence", {timeout:240000}, async () => published({git:true}, async selected => {
  const {page, pin, server, requests} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const method = (await outline(selected, "Watcher.java")).items.find(item => item.symbol.name === "measured");
  assert.ok(method); await selectSymbol(page, method);
  await page.waitForFunction(() => document.querySelector("#files-open-full")?.disabled === false);
  await page.locator("#files-working").click();
  await page.locator("#files-diff .files-diff-hunk").first().waitFor();
  assert.equal(await page.locator("#source").isVisible(), false, "Working tree has no visible indexed source to capture");
  assert.equal(await page.evaluate(() => location.hash), "");
  const sourceCount = sourceReads(requests), sequenceCount = sequenceReads(requests);
  await page.locator("#files-open-full").click();
  await page.waitForFunction(() => location.hash.startsWith("#sequence?") && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  const hash = await page.evaluate(() => location.hash), historyLength = await page.evaluate(() => history.length);
  const route = new URLSearchParams(hash.slice("#sequence?".length));
  assert.equal(route.get("sid"), method.symbol.id); assert.equal(route.get("rootKey"), server.rootKey);
  assert.equal(route.get("indexGeneration"), pin.indexGeneration); assert.equal(route.get("indexRevision"), String(pin.indexRevision));
  await page.goBack();
  await page.waitForFunction(() => location.hash === "" && !document.querySelector("#error")?.hidden && document.querySelector("#error")?.textContent.includes("no saved source view"));
  assert.equal(await page.locator("#error").isVisible(), true);
  assert.match(await page.locator("#error").textContent(), /current diagram remains open.*Forward/);
  assert.equal(await page.locator("#sequence-diagram").isVisible(), true);
  assert.equal(sourceReads(requests), sourceCount); assert.equal(sequenceReads(requests), sequenceCount);
  assert.equal(await page.evaluate(() => history.length), historyLength, "Back does not append a replacement route");
  await page.goForward();
  await page.waitForFunction(hash => location.hash === hash && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered") && document.querySelector("#error")?.hidden, hash);
  assert.equal(await page.locator("#sequence-diagram").isVisible(), true);
  assert.equal(sourceReads(requests), sourceCount); assert.equal(sequenceReads(requests), sequenceCount, "Forward reuses the exact admitted mini sequence");
  assert.equal(await page.evaluate(() => history.length), historyLength);
}));


const navigationSource = source.replace("  int measured", `${Array.from({length:60}, (_, i) => `  // Context ${i + 1}`).join("\n")}\n  int measured`);
test("Files declaration Sequence and browser traversal restore the exact pinned source position", {timeout:240000}, async () => published({sourceText:navigationSource}, async selected => {
  const {page, pin, server, requests} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const method = (await outline(selected, "Watcher.java")).items.find(item => item.symbol.name === "measured");
  assert.ok(method);
  await selectSymbol(page, method); await assertSource(page, navigationSource, method.symbol.range);
  const action = page.locator(`.source-diagram-action[data-symbol-id="${method.symbol.id}"]`);
  await action.waitFor(); await action.focus();
  await page.locator("#source").focus(); await page.keyboard.press("Home");
  for (let line = 1; line < method.symbol.range.startLine; line++) await page.keyboard.press("ArrowDown");
  await action.scrollIntoViewIfNeeded(); await action.focus();
  await action.evaluate(button => button.addEventListener("pointerdown", () => {
    const panel = document.querySelector("#workspace-source-panel");
    window.filesSequenceOrigin = {scroll:{top:panel.scrollTop, left:panel.scrollLeft},
      line:document.querySelector("#source").dataset.navigationLine, focusedSymbol:document.activeElement?.dataset.symbolId};
  }, {once:true}));
  const sourceCount = sourceReads(requests);
  const sequenceResponse = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === method.symbol.id);
  await action.click();
  const origin = await page.evaluate(() => window.filesSequenceOrigin), scroll = origin.scroll;
  assert.ok(scroll.top > 0, "native pointer interaction starts at a real source-panel scroll position");
  assert.equal(origin.line, String(method.symbol.range.startLine)); assert.equal(origin.focusedSymbol, method.symbol.id);
  const response = await sequenceResponse, dto = await response.json();
  assert.equal(response.status(), 200, JSON.stringify(dto));
  assert.equal(dto.seed.id, method.symbol.id); assert.deepEqual(dto.revision, pin);
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered") && location.hash.startsWith("#sequence?"));
  const route = new URLSearchParams((await page.evaluate(() => location.hash)).slice("#sequence?".length));
  assert.equal(route.get("sid"), method.symbol.id); assert.equal(route.get("rootKey"), server.rootKey);
  assert.equal(route.get("indexGeneration"), pin.indexGeneration); assert.equal(route.get("indexRevision"), String(pin.indexRevision));
  assert.equal(route.has("token"), false);
  const historyLength = await page.evaluate(() => history.length);
  async function returned() {
    await page.waitForFunction(line => document.body.classList.contains("files-section") && document.querySelector("#source")?.dataset.navigationLine === String(line), method.symbol.range.startLine);
    // Source paints before the async mini render; restored focus marks completion.
    await page.waitForFunction(({id, scroll}) => {
      const panel = document.querySelector("#workspace-source-panel"), focus = document.activeElement;
      return focus?.dataset.symbolId === id && focus.classList.contains("source-diagram-action") &&
        Math.abs(panel.scrollTop - scroll.top) <= 1 && panel.scrollLeft === scroll.left;
    }, {id:method.symbol.id, scroll});
    await assertSource(page, navigationSource, method.symbol.range);
    assert.equal(await page.locator("#files-indexed").getAttribute("aria-pressed"), "true");
    assert.equal(await page.locator(`.files-symbol-pick[data-symbol-id="${method.symbol.id}"]`).getAttribute("aria-pressed"), "true");
    assert.equal(await page.locator("#files-symbol-title").textContent(), method.symbol.name);
    assert.equal(sourceReads(requests), sourceCount, "Back reuses the captured source snapshot");
    assert.equal(await page.evaluate(() => history.length), historyLength, "traversal never appends history");
  }
  await page.goBack(); await returned();
  await page.goForward();
  await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered") && !document.body.classList.contains("files-section"), method.symbol.id);
  assert.equal(await page.evaluate(() => history.length), historyLength);
  await page.goBack(); await returned();
}));


const pagedNavigationSource = `class Watcher {
${Array.from({length:201}, (_, i) => `  int method${i}() { return ${i}; }`).join("\n")}
}
`;
test("explicit More source actions admits page two and Back restores its action without refetching metadata", {timeout:240000}, async () => published({sourceText:pagedNavigationSource}, async selected => {
  const {page, pin, requests} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const first = await outline(selected, "Watcher.java");
  assert.equal(first.items.length, 200); assert.ok(first.nextOffset > 0);
  const query = new URLSearchParams({...pin, path:"Watcher.java", offset:first.nextOffset, limit:200});
  const second = await read(selected, `symbols?${query}`);
  const target = second.items.find(item => item.symbol.name === "method200");
  assert.ok(target, "real metadata producer places the final ordinary callable on page two");
  assert.ok(!first.items.some(item => item.symbol.id === target.symbol.id));
  const sourceActions = page.locator(".source-navigation");
  const more = sourceActions.getByRole("button", {name:"More source actions", exact:true});
  await more.waitFor();
  await page.waitForFunction(() => [...document.querySelectorAll(".source-navigation button")].some(button => button.textContent === "More source actions" && !button.disabled && !button.hidden));
  const metadata = () => requests.filter(row => row.url.pathname.endsWith("/symbols"));
  const action = page.locator(`.source-diagram-action[data-symbol-id="${target.symbol.id}"]`);
  assert.equal(await action.count(), 0, "unloaded declarations have no guessed action");
  assert.equal(metadata().filter(row => Number(row.url.searchParams.get("offset")) > 0).length, 0, "source annotations never traverse remaining metadata automatically");
  const nextPage = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/symbols") && new URL(response.url()).searchParams.get("offset") === String(first.nextOffset));
  await more.click();
  const response = await nextPage, admitted = await response.json();
  assert.equal(response.status(), 200, JSON.stringify(admitted)); assert.deepEqual(admitted.revision, pin);
  assert.ok(admitted.items.some(item => item.symbol.id === target.symbol.id));
  await action.waitFor(); await action.scrollIntoViewIfNeeded(); await action.focus();
  const scroll = await page.locator("#workspace-source-panel").evaluate(panel => ({top:panel.scrollTop, left:panel.scrollLeft}));
  assert.ok(scroll.top > 0, "page-two action has a real source-panel position");
  const metadataCount = metadata().length, sourceCount = sourceReads(requests);
  const sequence = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === target.symbol.id);
  await action.click();
  const selectedResponse = await sequence, dto = await selectedResponse.json();
  assert.equal(selectedResponse.status(), 200, JSON.stringify(dto));
  assert.equal(dto.seed.id, target.symbol.id); assert.deepEqual(dto.revision, pin);
  await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"), target.symbol.id);
  await page.goBack();
  await page.waitForFunction(({id, line, scroll}) => {
    const panel = document.querySelector("#workspace-source-panel");
    return document.body.classList.contains("files-section") && document.querySelector("#source")?.dataset.navigationLine === String(line) &&
      document.activeElement?.dataset.symbolId === id && document.activeElement?.classList.contains("source-diagram-action") &&
      Math.abs(panel.scrollTop - scroll.top) <= 1 && panel.scrollLeft === scroll.left;
  }, {id:target.symbol.id, line:target.symbol.range.startLine, scroll});
  await assertSource(page, pagedNavigationSource);
  assert.equal(metadata().length, metadataCount, "Back reuses all explicitly admitted source-action metadata, including page two");
  assert.equal(sourceReads(requests), sourceCount, "Back reuses the pinned source snapshot");
  assert.equal(await more.isVisible(), false, "the restored completed metadata page has no More action");
}));

async function gitCommand(paths, ...args) { return H.run("git", args, {cwd:paths.workspace, env:paths.env}); }
const git = gitCommand;
async function prepareGit(paths, dirty = true) {
  await git(paths, "init", "-q");
  await git(paths, "config", "user.name", "Files acceptance");
  await git(paths, "config", "user.email", "files@example.invalid");
  writeFileSync(join(paths.workspace, "binary.bin"), Buffer.from([0, 1, 2]));
  writeFileSync(join(paths.workspace, "deleted.java"), "class Deleted {}\n");
  writeFileSync(join(paths.workspace, "rename-old.js"), "function renamed() { return 17; }\n");
  await git(paths, "add", "."); await git(paths, "commit", "-qm", "Files baseline");
  if (dirty) await dirtyGit(paths);
}
async function dirtyGit(paths) {
  writeFileSync(paths.source, source.replace("count = 1", "count = 10"));
  writeFileSync(join(paths.workspace, "clean.js"), "const clean = 2;\n");
  await git(paths, "add", "Watcher.java", "clean.js");
  writeFileSync(paths.source, source.replace("count = 1", "count = 10").replace("value + count", "value + count + 20"));
  writeFileSync(join(paths.workspace, "clean.js"), "const clean = 1;\n");
  writeFileSync(join(paths.workspace, "binary.bin"), Buffer.from([0, 9, 8]));
  rmSync(join(paths.workspace, "deleted.java"));
  renameSync(join(paths.workspace, "rename-old.js"), join(paths.workspace, "rename-new.js"));
  await git(paths, "add", "rename-old.js", "rename-new.js");
}

test("Files Working tree renders actual net HEAD staged and unstaged hunks without changing indexed evidence", {timeout:240000}, async () => published({git:true}, async selected => {
  const {page, requests} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const file = await cached(selected, "Watcher.java"), symbols = await outline(selected, "Watcher.java");
  const method = symbols.items.find(item => item.symbol.name === "measured");
  await selectSymbol(page, method);
  const status = await read(selected, "git");
  assert.equal(status.git.state, "available");
  assert.ok(!status.changes.some(row => row.path === "clean.js"), "an intermediate staged change restored to HEAD is net unchanged");
  const diff = await read(selected, "git/diff?path=Watcher.java");
  assert.equal(diff.file.status, "modified"); assert.equal(diff.file.binary, false);
  const added = diff.file.hunks.flatMap(hunk => hunk.lines).filter(line => line.kind === "added").map(line => line.text).join("");
  assert.match(added, /count = 10/); assert.match(added, /value \+ count \+ 20/, "both staged and unstaged edits belong to HEAD→working tree");
  const reads = sourceReads(requests);
  await page.locator("#files-working").click();
  await page.waitForFunction(count => document.querySelectorAll("#files-diff .files-diff-hunk").length === count, diff.file.hunks.length);
  assert.match(await page.locator("#files-basis-note").textContent(), /uncommitted.*not indexed/i);
  assert.equal(await page.locator("#files-working").getAttribute("aria-pressed"), "true");
  const headers = await page.locator("#files-diff .files-diff-hunk").allTextContents();
  assert.deepEqual(headers.map(value => value.trimEnd()), diff.file.hunks.map(hunk => hunk.header.trimEnd()));
  const lines = await page.locator("#files-diff .files-diff-line").evaluateAll(rows => rows.map(row => ({
    kind:row.dataset.kind, marker:row.querySelector(".files-diff-mark").textContent, text:row.querySelector(".files-diff-text").textContent,
  })));
  assert.deepEqual(lines, diff.file.hunks.flatMap(hunk => hunk.lines.map(line => ({kind:line.kind, marker:line.kind === "added" ? "+" : line.kind === "deleted" ? "−" : " ", text:line.text}))));
  assert.equal(await page.locator("#files-symbol-title").textContent(), method.symbol.name, "working-tree display does not fabricate a new outline basis");
  assert.equal(sourceReads(requests), reads, "Git basis never substitutes a live source request");
  const navBefore = requests.filter(row => row.url.pathname.endsWith("/navigation")).length, sequenceBefore = sequenceReads(requests);
  assert.equal(await page.locator("#files-diff .source-diagram-action").count(), 0, "working-tree-only source coordinates have no indexed declaration actions");
  await page.locator("#files-diff .files-diff-line").first().click(); await page.keyboard.press("Shift+F10"); await page.keyboard.press("Escape");
  assert.equal(requests.filter(row => row.url.pathname.endsWith("/navigation")).length, navBefore, "Git coordinates are not indexed navigation coordinates");
  assert.equal(sequenceReads(requests), sequenceBefore, "working-tree source never seeds a sequence");
  await openFile(selected, "clean.js");
  const diffReads = requests.filter(row => row.url.pathname.endsWith("/git/diff")).length;
  await page.locator("#files-working").click();
  await page.waitForFunction(() => document.querySelector("#files-diff")?.textContent.includes("No uncommitted changes"));
  assert.equal(requests.filter(row => row.url.pathname.endsWith("/git/diff")).length, diffReads, "unchanged paths do not request a diff");
  for (const path of ["binary.bin", "deleted.java"]) {
    const expected = await read(selected, `git/diff?path=${encodeURIComponent(path)}`);
    await page.locator(`.files-change-pick[data-path="${path}"]`).click();
    await page.waitForFunction(path => document.querySelector("#files-diff .files-diff-status")?.textContent.includes(path), path);
    assert.equal(await page.locator("#files-indexed").isEnabled(), false);
    assert.equal(await page.locator("#files-symbol-header").textContent(), "", "changed-only files have no invented indexed declaration");
    assert.match(await page.locator("#source-path").textContent(), /no indexed outline or navigation/);
    if (expected.file.binary) {
      assert.match(await page.locator("#files-diff").textContent(), /Binary file.*no textual hunks/);
      assert.equal(await page.locator("#files-diff .files-diff-line").count(), 0);
    } else {
      assert.equal(expected.file.status, "deleted");
      assert.deepEqual(await page.locator("#files-diff .files-diff-text").allTextContents(), expected.file.hunks.flatMap(hunk => hunk.lines.map(line => line.text)));
    }
  }
  await openFile(selected, "rename-new.js");
  await page.locator("#files-working").click();
  await page.waitForFunction(() => document.querySelector("#files-diff .files-diff-status")?.textContent.includes("rename-new.js"));
  const rename = await read(selected, "git/diff?path=rename-new.js");
  assert.equal(rename.file.status, "renamed"); assert.equal(rename.file.previousPath, "rename-old.js");
  assert.ok((await page.locator("#files-diff .files-diff-status").textContent()).includes(`${rename.file.previousPath} → ${rename.file.path}`));
  await openFile(selected, "Watcher.java"); await selectSymbol(page, method);
  await page.locator("#files-working").click();
  await page.waitForFunction(() => document.querySelector("#files-diff .files-diff-line"));
  await page.setViewportSize({width:390, height:844});
  assert.equal(await page.locator("#files-working").getAttribute("aria-pressed"), "true");
  assert.deepEqual(await page.locator("#files-diff .files-diff-hunk").allTextContents(), diff.file.hunks.map(hunk => hunk.header));
  await assertNoOverflow(page);
  await page.locator("#files-indexed").click(); await assertSource(page, file.text, method.symbol.range);
  await page.locator("#files-working").click();
  await page.waitForFunction(() => document.querySelector("#files-diff .files-diff-line"));
  await assertNoOverflow(page);
  for (const row of requests.filter(row => /\/git(?:\/diff)?$/.test(row.url.pathname))) {
    assert.equal(row.method, "GET"); assert.equal(row.url.searchParams.has("indexRevision"), false);
  }
  await assertNoOverflow(page);
}));

test("390px Files keeps cached noncallable source, safe labels, no-Git reason and source keyboard navigation", {timeout:240000}, async () => published({width:390}, async selected => {
  const {page, requests, pin} = selected;
  await enterFiles(selected); await openFile(selected, "Watcher.java");
  const dto = await outline(selected, "Watcher.java"), type = dto.items.find(item => item.nativeKind === "type");
  await selectSymbol(page, type);
  assert.equal(await page.locator("#files-working").isVisible(), false);
  assert.match(await page.locator("#files-git-state").textContent(), /not_repository|not a Git repository|not a repository/i);
  await assertSource(page, source, type.symbol.range);
  await page.evaluate(() => {
    window.filesNavigationTrace = [];
    const record = (kind, detail) => { if (window.filesNavigationTrace.length < 64) window.filesNavigationTrace.push({kind, time:performance.now(), ...detail}); };
    document.addEventListener("scroll", event => record("scroll", {target:event.target.id || event.target.nodeName, top:event.target.scrollTop, left:event.target.scrollLeft}), true);
    document.addEventListener("focusin", event => record("focus", {target:event.target.id || event.target.textContent?.slice(0, 120)}));
    document.addEventListener("keydown", event => {
      if (event.key === "F10" && event.shiftKey) record("navigation-key", {scroll:[...document.querySelectorAll("#source, #workspace-source-panel, #source-dock, #file-tree")].map(node => ({id:node.id, top:node.scrollTop, left:node.scrollLeft}))});
    }, true);
    new MutationObserver(records => {
      for (const record_ of records) for (const node of [...record_.addedNodes, ...record_.removedNodes]) {
        if (node.nodeType === 1 && node.classList.contains("classes-context-menu")) record("menu", {connected:node.isConnected, text:node.textContent});
      }
    }).observe(document.body, {childList:true});
  });
  await page.locator("#source").focus(); await page.keyboard.press("Home"); await page.keyboard.press("ArrowDown");
  assert.match(await page.locator("#source").getAttribute("aria-label"), /Active line 2/);
  const navigation = page.waitForRequest(request => new URL(request.url()).pathname.endsWith("/navigation"));
  const navigationResponse = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/navigation"));
  await page.keyboard.press("Shift+F10");
  const request = await navigation;
  assert.deepEqual(request.postDataJSON(), {expectedRevision:pin, path:"Watcher.java", line:2, semanticProducerPolicy:"solePublished"});
  const response = await navigationResponse, navigationDto = await response.json();
  assert.equal(response.status(), 200, JSON.stringify(navigationDto));
  assert.deepEqual(navigationDto.revision, pin);
  assert.ok(navigationDto.targets.some(target => target.reason === "declaration" && target.matchKind === "measured" && target.symbol.id === type.symbol.id && target.symbol.path === "Watcher.java" && target.symbol.range.startLine === 2),
    `class declaration must be navigable on its exact source line: ${JSON.stringify(navigationDto)}`);
  try {
    await page.getByRole("menuitem", {name:/Read declaration source/}).waitFor();
  } catch (error) {
    const presentation = await page.evaluate(() => ({menus:[...document.querySelectorAll(".classes-context-menu")].map(menu => ({text:menu.textContent, items:[...menu.querySelectorAll('[role="menuitem"]')].map(item => ({text:item.textContent, disabled:item.disabled})), rect:menu.getBoundingClientRect().toJSON()})),
      focused:document.activeElement?.outerHTML, sourceLabel:document.querySelector("#source")?.getAttribute("aria-label"), trace:window.filesNavigationTrace}));
    assert.fail(`Published navigation did not expose its measured declaration. DTO=${JSON.stringify(navigationDto)} UI=${JSON.stringify(presentation)} Failure=${error.message}`);
  }
  assert.ok((await page.locator('.classes-context-menu [role="menuitem"]').allTextContents()).some(label => label.includes("Read declaration source") && label.includes(type.symbol.name) && label.includes("Watcher.java:2")));
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#source").evaluate(node => document.activeElement === node), true);
  await page.locator("#explorer-toggle").click();
  assert.equal(await page.locator("#inspector-toggle").getAttribute("aria-expanded"), "false");
  const box = await page.locator("#explorer-panel").boundingBox();
  assert.ok(box && box.x >= 0 && box.x + box.width <= 391);
  await page.locator("#inspector-toggle").click();
  assert.equal(await page.locator("#explorer-toggle").getAttribute("aria-expanded"), "false");
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#inspector-toggle").evaluate(node => document.activeElement === node), true);
  await openFile(selected, hostileName);
  await assertSource(page, (await cached(selected, hostileName)).text);
  assert.equal(await page.locator("#file-tree img, #source-path img, #source script").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.filenameExecuted || globalThis.sourceExecuted), undefined);
  assert.equal(await page.locator(".files-symbol-pick").filter({hasText:"safe"}).count(), 0, "a top-level variable is not an outline row (#526)");
  assert.equal(await page.locator("#file-tree li.detail").filter({hasText:"No types or callables. Cached source remains readable."}).count(), 1);
  assert.equal(sequenceReads(requests), 0, "types and file selection never invent a callable sequence");
  await assertNoOverflow(page);
}));


test("measured Git counts and exact M/A/D/R markers preserve indexed evidence boundaries", {timeout:240000}, async () => published({git:"markers"}, async selected => {
  const {page, paths, server, requests} = selected;
  const status = await read(selected, "git");
  assert.equal(status.git.state, "available");
  assert.deepEqual(status.git.workingTree, {state:"available", changedFiles:7});
  assert.equal(status.changes.length, 7, "a rename counts once and restored staged content is net unchanged");
  assert.deepEqual([status.git.upstream.ahead, status.git.upstream.behind], [1, 1]);
  const expected = `${status.git.head.name} · 7 uncommitted · origin/main ↑1 ↓1 · As of last fetch`;
  await page.waitForFunction(text => document.querySelector("#workspace-git")?.textContent === text, expected);
  const marker = async (path, status, letter) => {
    const found = await page.locator("#file-tree .git-marker").evaluateAll((nodes, path) => nodes.filter(node => node.dataset.path === path).map(node => ({
      text:node.textContent, status:node.dataset.status, title:node.title, label:node.getAttribute("aria-label"),
    })), path);
    assert.equal(found.length, 1, `one exact marker for ${path}: ${JSON.stringify(await page.locator("#file-tree .file-pick, #file-tree .files-change-pick").evaluateAll(nodes => nodes.map(node => ({path:node.title || node.dataset.path, text:node.textContent}))))}`);
    assert.equal(found[0].status, status); assert.equal(found[0].text, letter);
    assert.match(found[0].title, /uncommitted vs HEAD/); assert.equal(found[0].label, found[0].title);
    return found[0];
  };
  await page.locator('.directory-pick[title="changed/"]').click();
  await page.locator('.directory-pick[title="unchanged/"]').click();
  await marker("changed/same.js", "modified", "M");
  assert.equal(await page.locator('.file-pick[title="unchanged/same.js"] .git-marker').count(), 0, "basename matches are not Git matches");
  await marker("added.js", "added", "A");
  const liveRename = await marker("rename-new.js", "renamed", "R");
  assert.ok(liveRename.title.includes("rename-old.js"));
  assert.equal(await page.locator('.file-pick[title="deleted.java"]').count(), 0, "a deletion does not invent a live file");
  await enterFiles(selected);
  await page.waitForFunction(() => /\d+ of \d+ loaded files/.test(document.querySelector("#files-state")?.textContent || ""));
  await marker("Watcher.java", "modified", "M");
  await marker("added.js", "added", "A");
  await marker("deleted.java", "deleted", "D");
  const rename = await marker("rename-new.js", "renamed", "R");
  assert.ok(rename.title.includes("rename-old.js"));
  assert.equal(await page.locator('.file-pick[title="rename-old.js"]').count(), 0, "the catalog does not invent a cached rename source");
  await marker(hostileName, "modified", "M");
  assert.equal(await page.locator('#file-tree img, #file-tree svg').count(), 0);
  assert.equal(await page.evaluate(() => globalThis.filenameExecuted), undefined);
  await openFile(selected, "Watcher.java");
  await assertSource(page, (await cached(selected, "Watcher.java")).text);
  assert.equal(await page.locator("#files-indexed").isEnabled(), true, "Git decoration retains actual indexed source access");
  const before = sourceReads(requests);
  await page.locator('.files-change-pick[data-path="deleted.java"]').click();
  await page.waitForFunction(() => document.querySelector("#files-diff .files-diff-status")?.textContent.includes("deleted.java"));
  assert.equal(await page.locator("#files-indexed").isEnabled(), false);
  assert.equal(await page.locator("#files-symbol-header").textContent(), "");
  assert.equal(sourceReads(requests), before, "changed-only paths do not fetch indexed source");
  await page.locator("#file-filter").fill("same.js");
  assert.equal(await page.locator("#workspace-git").textContent(), expected, "filtering never changes the checkout-wide count");
  assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false);
  const listing = await H.api(server.base, paths.token, "GET", "/api/checkouts");
  assert.deepEqual(listing.data.checkouts.find(row => row.rootKey === server.rootKey).git.workingTree, status.git.workingTree);
}));
