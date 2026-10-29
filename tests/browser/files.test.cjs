"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {writeFileSync, rmSync, renameSync} = require("node:fs");
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

async function published({git = false, width = 1440, sourceText = source} = {}, callback) {
  const paths = H.fixture(sourceText, "Watcher.java");
  writeFileSync(join(paths.workspace, "Uses.java"), uses);
  writeFileSync(join(paths.workspace, "clean.js"), "const clean = 1;\n");
  writeFileSync(join(paths.workspace, hostileName), "// <script>globalThis.sourceExecuted=true</script>\nconst safe = 'λ';\n");
  let server, browser, context;
  try {
    if (git) await prepareGit(paths);
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = JSON.parse(indexed.stdout).status.revision;
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
async function selectSymbol(page, item) {
  if (await page.locator("#explorer-toggle").isVisible() && await page.locator("#explorer-toggle").getAttribute("aria-expanded") !== "true") await page.locator("#explorer-toggle").click();
  const row = page.locator(`.files-symbol-pick[data-symbol-id="${item.symbol.id}"]`);
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

test("published Files keeps original all-kind outline, UTF-8 syntax locations and mini/full sequence on one pin", {timeout:240000}, async () => published({}, async selected => {
  const {page, pin, requests} = selected;
  await enterFiles(selected);
  await openFile(selected, "Watcher.java");
  const dto = await outline(selected, "Watcher.java"), file = await cached(selected, "Watcher.java");
  assert.equal(file.text, source);
  const kinds = new Set(dto.items.map(item => item.nativeKind));
  for (const kind of ["type", "field", "constructor", "method", "parameter"]) assert.ok(kinds.has(kind), `real Java producer emits ${kind}`);
  await page.waitForFunction(count => document.querySelectorAll(".files-symbol-pick[data-symbol-id]").length === count, dto.items.length);
  assert.deepEqual(await page.locator(".files-symbol-pick").evaluateAll(rows => rows.map(row => row.dataset.symbolId)), dto.items.map(item => item.symbol.id));
  for (const item of dto.items) {
    const label = await page.locator(`.files-symbol-pick[data-symbol-id="${item.symbol.id}"]`).textContent();
    assert.ok(label.includes(item.symbol.name)); assert.ok(label.includes(item.nativeKind));
  }
  const type = dto.items.find(item => item.nativeKind === "type"), method = dto.items.find(item => item.symbol.name === "measured");
  assert.ok(type && method);
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
  assert.match(await page.locator("#files-matches-title").textContent(), /same spelling, not compiler-resolved/);
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


// #426: icon plus name on one line, with kind and line right-aligned.
async function assertOneLineRows(page, items) {
  await page.waitForFunction(count => document.querySelectorAll(".files-symbol-pick[data-symbol-id]").length === count, items.length);
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
  const highlighted = await page.locator("#source .source-line.highlight").evaluateAll(rows => rows.map(row => getComputedStyle(row).color));
  assert.ok(highlighted.length && highlighted.every(color => color !== colours.gold && color !== colours.secondary), "the selected range keeps its own contrast");
  assert.equal((await syntaxClasses(page))[annotation].kind, "attribute", "selection keeps the line class and only changes its colour");

  await openFile(selected, hostileName);
  lines = await syntaxClasses(page);
  assert.equal(lines[0].kind, "comment", "a script-looking comment is coloured as text");
  assert.equal(await page.locator("#source script, #source img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.sourceExecuted), undefined);
  await assertSource(page, (await cached(selected, hostileName)).text);

  // Diagrams reuses the cached snapshot without Files colouring.
  await page.locator("#section-diagrams").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section"));
  assert.equal(await page.locator("#source .syntax-comment, #source .syntax-attribute").count(), 0);
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
    await assertSource(page, navigationSource, method.symbol.range);
    assert.equal(await page.locator("#files-indexed").getAttribute("aria-pressed"), "true");
    assert.equal(await page.locator(`.files-symbol-pick[data-symbol-id="${method.symbol.id}"]`).getAttribute("aria-pressed"), "true");
    assert.equal(await page.locator("#files-symbol-title").textContent(), method.symbol.name);
    await page.waitForFunction(({id, scroll}) => {
      const panel = document.querySelector("#workspace-source-panel"), focus = document.activeElement;
      return focus?.dataset.symbolId === id && focus.classList.contains("source-diagram-action") &&
        Math.abs(panel.scrollTop - scroll.top) <= 1 && panel.scrollLeft === scroll.left;
    }, {id:method.symbol.id, scroll});
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

async function git(paths, ...args) { return H.run("git", args, {cwd:paths.workspace, env:paths.env}); }
async function prepareGit(paths) {
  await git(paths, "init", "-q");
  await git(paths, "config", "user.name", "Files acceptance");
  await git(paths, "config", "user.email", "files@example.invalid");
  writeFileSync(join(paths.workspace, "binary.bin"), Buffer.from([0, 1, 2]));
  writeFileSync(join(paths.workspace, "deleted.java"), "class Deleted {}\n");
  writeFileSync(join(paths.workspace, "rename-old.js"), "function renamed() { return 17; }\n");
  await git(paths, "add", "."); await git(paths, "commit", "-qm", "Files baseline");
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
  assert.equal(await page.locator(".files-symbol-pick").filter({hasText:"safe"}).count(), 1, "a file with no callables still has a measured variable outline");
  assert.equal(sequenceReads(requests), 0, "types and file selection never invent a callable sequence");
  await assertNoOverflow(page);
}));
