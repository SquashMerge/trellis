"use strict";
// Pele class adapter and controller in real Chromium under the product CSP.
// Synthetic class DTOs exercise identity, text safety, validation and actions;
// classes-app.test.cjs covers the published daemon response.
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const pin = readFileSync(join(ROOT, "web/app.js"), "utf8").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const files = {"/vendor/pele-1.10.0-trellis.2.js": "web/vendor/pele-1.10.0-trellis.2.js", "/classes.js": "web/classes.js", "/style.css": "web/style.css", "/classes.css": "web/classes.css"};
const html = `<!doctype html><html><head><link rel="stylesheet" href="/style.css"><link rel="stylesheet" href="/classes.css">
<script src="/pin.js" defer></script><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/classes.js" defer></script></head>
<body><section id="classes-panel"><p id="classes-state"></p><input id="classes-query"><button id="classes-search">Search</button><input id="classes-unmatched" type="checkbox">
<div id="classes-results"></div><div id="classes-diagram"></div></section></body></html>`;
const policy = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
let server, base, browser, page, errors;
test.before(async () => {
  server = createServer((req, res) => {
    res.setHeader("Content-Security-Policy", policy);
    if (req.url === "/pin.js") { res.setHeader("Content-Type", "text/javascript; charset=utf-8"); res.end(pin); return; }
    const file = files[req.url];
    if (file) { res.setHeader("Content-Type", file.endsWith("css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8"); res.end(readFileSync(join(ROOT, file))); return; }
    if (req.url === "/") { res.setHeader("Content-Type", "text/html"); res.end(html); return; }
    res.writeHead(404); res.end();
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done)); base = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch(); page = await browser.newPage(); errors = [];
  page.on("console", msg => { if (msg.type() === "error") errors.push(msg.text()); }); page.on("pageerror", e => errors.push(e.message));
  await page.goto(base); await page.waitForFunction(() => !!window.TrellisPeleClasses && !!window.pele && !!window.TrellisClasses);
  // One controller per page, as in the app. Each test swaps the DTO and pin.
  await page.evaluate(() => {
    window.harness = {calls: [], reads: [], methods: [], navigations: [], revision: {indexGeneration: "12345678-1234-4123-8123-123456789abc", indexRevision: 1}, dto: null};
    const h = window.harness;
    window.TrellisClasses.init({
      request: async (url, options) => { h.calls.push(url); return options ? structuredClone(h.dto) : {revision: h.revision, items: [], warnings: []}; },
      readSource: (item, revision) => h.reads.push({item, revision}), selectMethod: symbol => h.methods.push(symbol),
      navigateMember: (event, selector) => h.navigations.push(selector),
      currentRevision: () => h.revision, currentSession: () => "one", onChange() {}, onStale() {},
    });
  });
});
test.after(async () => { await browser?.close(); await new Promise(done => server?.close(done)); });
const revision = {indexGeneration: "12345678-1234-4123-8123-123456789abc", indexRevision: 1};
const range = {startLine: 2, endLine: 9, startByte: 10, endByte: 200};
function definition(id, name, extra = {}) {
  return {symbol: {id, name, path: `src/${id}.java`, range, provenance: {parser: "syntax"}}, qualifiedName: `sample.${name}`, language: "java", declarationKind: "class",
    fields: [{name: "count", typeHint: "int", path: `src/${id}.java`, range: {...range, startByte: 20, endByte: 30}}],
    methods: [{name: "run", symbolId: `${id}#run`, typeHint: "void", path: `src/${id}.java`, range: {...range, startByte: 40, endByte: 90}},
      {name: "helper", symbolId: `${id}#helper`, typeHint: null, path: `src/${id}.java`, range: {...range, startByte: 100, endByte: 150}}], truncated: false, ...extra};
}
const diagram = (nodes, seed = nodes[0].id) => ({revision, seed, nodes, edges: [], warnings: [], truncated: false});
const node = (id, name = id, extra) => ({id, label: name, kind: "class", expandable: true, class: definition(id, name, extra)});
async function open(dto, seed = dto.seed) {
  await page.evaluate(async ({dto, seed}) => {
    const h = window.harness; Object.assign(h, {dto, calls: [], reads: [], methods: [], navigations: [], revision: dto.revision});
    document.querySelector(".classes-context-menu")?.remove();
    await window.TrellisClasses.open({seed});
  }, {dto, seed});
}
const state = () => page.locator("#classes-state").textContent();
// Click the class box's header corner, clear of member lines.
const classMenu = id => page.locator(`.pele-node[data-trellis-class-id="${id}"] > rect`).click({position: {x: 10, y: 10}});
const cspErrors = () => errors.filter(error => /Content Security Policy|CSP/i.test(error));

test("Pele draws each measured class with native IDs mapped back to Trellis class IDs", async () => {
  await open(diagram([node("A", "Alpha"), node("B", "Beta")]));
  assert.match(await state(), /Pele rendered/);
  assert.equal(await page.locator("#classes-diagram svg.pele.pele-class.trellis-pele").count(), 1);
  assert.equal(await page.locator("#classes-diagram .classes-node").count(), 0, "no card fallback");
  // Only the focus is selected by default; "Show all" adds the other returned class.
  assert.deepEqual(await page.locator(".pele-node").evaluateAll(nodes => nodes.map(n => [n.dataset.id, n.dataset.trellisClassId])), [["C0", "A"]]);
  await page.getByRole("button", {name: "Show all returned classes"}).click();
  assert.deepEqual(await page.locator(".pele-node").evaluateAll(nodes => nodes.map(n => [n.dataset.id, n.dataset.trellisClassId, n.classList.contains("trellis-class-seed")])),
    [["C0", "A", true], ["C1", "B", false]]);
  assert.equal(await page.locator(".pele-node .pele-label").first().textContent(), "Alpha");
  assert.equal(await page.locator(".pele-node .pele-annotation").first().textContent(), "«java · class · focus · 1 field · 2 methods»");
  assert.equal(await page.locator(".pele-edge, .pele-edge-label").count(), 0, "no relation is drawn");
  assert.equal(await page.locator(".pele-member").count(), 0, "members start collapsed");
  assert.equal(await page.locator("#classes-diagram svg [style]").count() + await page.locator("#classes-diagram svg[style]").count(), 0);
  assert.deepEqual(cspErrors(), []);
});

test("repository text is shown literally through encoded labels and never becomes Mermaid or markup", async () => {
  const hostile = 'Evil<T> & "q" {x} </text><script>window.pwned=1</script>';
  const dto = diagram([node("H", hostile, {fields: [{name: "f\n<<interface>>", typeHint: "Map<K, V> | None", path: "src/H.java", range}],
    methods: [{name: "m$", symbolId: "H#m", typeHint: "List[int] *", path: "src/H.java", range}, {name: "x()]", symbolId: null, typeHint: "", path: "src/H.java", range}]})]);
  await open(dto);
  const source = await page.evaluate(dto => window.TrellisPeleClasses.render(dto.nodes, {seed: "H", membersOpen: new Set(["H"])}).source, dto);
  for (const raw of ["<T>", "&", "\"q\"", "{x}", "<script", "<<interface>>", "Map<", "m$", "x()]"]) assert.ok(!source.includes(raw), `encoded: ${raw}`);
  await classMenu("H");
  await page.getByRole("menuitem", {name: /Show members/}).click();
  assert.equal(await page.locator(".pele-node .pele-label").textContent(), Array.from(hostile).length > 60 ? hostile.slice(0, 59) + "…" : hostile);
  assert.deepEqual(await page.locator(".pele-member").allTextContents(), ["f <<interface>>: Map<K, V> | None", "m$() : List[int] *", "x()]()"]);
  assert.deepEqual(await page.locator(".pele-member").evaluateAll(n => n.map(x => x.dataset.trellisMemberKind)), ["field", "method", "method"]);
  assert.equal(await page.evaluate(() => window.pwned), undefined);
  assert.equal(await page.locator("#classes-diagram script, #classes-diagram image, #classes-diagram foreignObject").count(), 0);
  assert.match(await page.locator(".pele-node").getAttribute("aria-label"), /sample\.Evil<T> & "q"/);
  assert.deepEqual(cspErrors(), []);
});

test("class menu, members, method and member navigation act on original DTO records with mouse and keyboard", async () => {
  await open(diagram([node("A", "Alpha")]));
  const cls = page.locator('.pele-node[data-trellis-class-id="A"]');
  assert.equal(await cls.getAttribute("role"), "button"); assert.equal(await cls.getAttribute("aria-haspopup"), "menu");
  await cls.focus(); await page.keyboard.press("Enter");
  assert.deepEqual(await page.locator(".classes-context-menu [role=menuitem]").allTextContents(), ["Show members · 1 field · 2 methods", "Read class source", "Focus this class"]);
  assert.equal(await cls.getAttribute("aria-expanded"), "true");
  await page.keyboard.press("ArrowDown"); await page.keyboard.press("Enter");
  assert.deepEqual(await page.evaluate(() => window.harness.reads.map(r => [r.item.path, r.item.range.startLine, r.revision.indexRevision])), [["src/A.java", 2, 1]]);
  await classMenu("A"); await page.getByRole("menuitem", {name: "Show members · 1 field · 2 methods"}).click();
  assert.deepEqual(await page.locator(".pele-member").allTextContents(), ["count: int", "run() : void", "helper()"]);
  assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisClassId), "A", "focus returns to the redrawn class");
  const run = page.locator('.pele-member[data-trellis-symbol-id="A#run"]');
  await run.click(); await run.focus(); await page.keyboard.press("Enter");
  assert.deepEqual(await page.evaluate(() => window.harness.methods.map(m => [m.id, m.parent, m.path, m.range.startByte])), [["A#run", "A", "src/A.java", 40], ["A#run", "A", "src/A.java", 40]]);
  assert.equal(await page.locator(".classes-context-menu").count(), 0, "a member action does not open the class menu");
  await run.focus(); await page.keyboard.press("Shift+F10");
  const field = page.locator('.pele-member[data-trellis-member-kind="field"]');
  await field.click();
  assert.deepEqual(await page.evaluate(() => window.harness.navigations), [{classId: "A", memberName: "run", startByte: 40, endByte: 90}, {classId: "A", memberName: "count", startByte: 20, endByte: 30}]);
  await classMenu("A"); await page.getByRole("menuitem", {name: "Hide members"}).click();
  assert.equal(await page.locator(".pele-member").count(), 0);
  assert.deepEqual(cspErrors(), []);
});

test("actions are inert after the pinned revision changes", async () => {
  await open(diagram([node("A", "Alpha")]));
  await classMenu("A"); await page.getByRole("menuitem", {name: /Show members/}).click();
  await page.evaluate(() => { window.harness.revision = {...window.harness.revision, indexRevision: 2}; });
  await page.locator('.pele-member[data-trellis-symbol-id="A#run"]').click({force: true});
  await page.locator('.pele-member[data-trellis-member-kind="field"]').click({force: true});
  assert.deepEqual(await page.evaluate(() => [window.harness.methods.length, window.harness.navigations.length, window.harness.reads.length]), [0, 0, 0]);
});

test("validation fails closed and the view falls back visibly to cards", async () => {
  const dto = diagram([node("A", "Alpha"), node("B", "Beta")]);
  const reasons = await page.evaluate(dto => {
    const P = window.TrellisPeleClasses, open = new Set(["A"]);
    const projection = P.project(dto.nodes, {seed: "A", membersOpen: open});
    const raw = window.pele.render(projection.source, {fontSize: 14, responsive: false, links: false, images: false}).svg;
    const attempt = svg => { try { P.validatedSvg(svg, projection); return "ok"; } catch (error) { return error.message; } };
    const many = Array.from({length: 25}, (_, i) => ({...dto.nodes[0], id: `N${i}`, class: {...dto.nodes[0].class, symbol: {...dto.nodes[0].class.symbol, id: `N${i}`}}}));
    return {
      exact: attempt(raw),
      renamed: attempt(raw.replace('data-id="C1"', 'data-id="C9"')),
      reordered: attempt(raw.replace('data-id="C0"', 'data-id="Cx"').replace('data-id="C1"', 'data-id="C0"').replace('data-id="Cx"', 'data-id="C1"')),
      member: attempt(raw.replace(">count: int<", ">count: long<")),
      dropped: attempt(raw.replace(/<text class="pele-member pele-method"[^>]*>helper\(\)<\/text>/, "")),
      script: attempt(raw.replace("</svg>", "<script>1</script></svg>")),
      attribute: attempt(raw.replace('data-id="C0"', 'data-id="C0" onclick="x"')),
      note: attempt(raw.replace("</svg>", '<g class="pele-note"><rect x="0" y="0" width="1" height="1"/></g></svg>')),
      noteEdge: attempt(raw.replace("</svg>", '<path class="pele-note-edge" d="M0 0L1 1"/></svg>')),
      tooMany: P.render(many, {}).reason,
      bigSource: P.render([{...dto.nodes[0], class: {...dto.nodes[0].class, fields: Array.from({length: 200}, (_, i) => ({name: `f${i}<>`.repeat(8), typeHint: "<>".repeat(20), path: "a", range: dto.nodes[0].class.symbol.range}))}}], {membersOpen: new Set(["A"])}).reason,
      badWitness: P.render([{...dto.nodes[0], class: {...dto.nodes[0].class, symbol: {...dto.nodes[0].class.symbol, range: {startLine: 0, endLine: 1}}}}], {}).reason,
    };
  }, dto);
  assert.equal(reasons.exact, "ok");
  for (const key of ["renamed", "reordered", "member", "dropped", "script", "attribute", "note", "noteEdge"]) assert.notEqual(reasons[key], "ok", key);
  assert.equal(reasons.tooMany, "Too many classes for Pele");
  assert.equal(reasons.bigSource, "Class Mermaid source too large");
  assert.equal(reasons.badWitness, "Unsupported class declaration");
  // An unrepresentable declaration keeps the card renderer and says why.
  // Collapsed, it fits Pele; expanding its members exceeds Pele's source limit.
  await open(diagram([{...node("W", "Wide"), class: {...definition("W", "Wide"), fields: Array.from({length: 200}, (_, i) => ({name: `f${i}<>`.repeat(8), typeHint: "<>".repeat(20), path: "src/W.java", range}))}}]));
  assert.match(await state(), /Pele rendered/);
  await classMenu("W"); await page.getByRole("menuitem", {name: /Show members/}).click();
  assert.match(await state(), /Pele fell back: Class Mermaid source too large/);
  assert.equal(await page.locator("#classes-diagram svg.pele").count(), 0);
  assert.equal(await page.locator(".classes-node .classes-member").count(), 202);
  assert.equal(await page.locator(".classes-node .classes-members-toggle").getAttribute("aria-expanded"), "true");
  // Collapsing in the card view makes the class representable again.
  await page.locator(".classes-node .classes-members-toggle").click();
  assert.match(await state(), /Pele rendered/);
  assert.equal(await page.locator(".classes-node").count(), 0);
  assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisClassId), "W");
});

for (const [fields, methods, shown, omitted] of [
  [1, 203, "first 200 methods", 3],
  [203, 2, "first 200 fields", 3],
  [201, 202, "first 200 fields and first 200 methods", 3],
]) test(`member bounds disclose only overflowing lists: ${fields} fields and ${methods} methods`, async () => {
  const members = (count, kind) => Array.from({length: count}, (_, i) => ({name: `${kind}${i}`, typeHint: null, path: "src/P.java", range}));
  await open(diagram([node("P", "Partial", {fields: members(fields, "f"), methods: members(methods, "m"), truncated: true})]));
  await classMenu("P"); await page.getByRole("menuitem", {name: /Show members/}).click();
  assert.match(await state(), /Pele rendered/);
  assert.equal(await page.locator(".pele-member.pele-attribute").count(), Math.min(fields, 200));
  assert.equal(await page.locator(".pele-member.pele-method").count(), Math.min(methods, 200));
  assert.deepEqual(await page.locator(".classes-pele-notices li").allTextContents(), [
    `sample.Partial: ${shown} shown; ${omitted} more omitted. Read class source for more.`,
    "sample.Partial: partial declaration · read source for details."]);
});
