"use strict";
// #558: large sequences stay readable. Real Pele, real CSS and real geometry in Chromium, over
// a DTO shaped like the `coordinate` method; the store's columns are tested in tests/mcp.
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js", "/sequence-pele.js":"web/sequence-pele.js", "/sequence.js":"web/sequence.js", "/style.css":"web/style.css"};
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><div id="sequence-diagram" class="sequence-scroll" style="flex:none;width:900px;height:500px"></div><div id="files-mini-sequence" class="sequence-scroll" style="flex:none;width:300px;height:200px"></div></body></html>';
const baseView = () => JSON.parse(readFileSync(join(ROOT, "tests/fixtures/sequence-pele/rust.json")));
let server, browser, page;

test.before(async () => {
  server = createServer((req, res) => {
    const file = files[req.url];
    if (file) { res.setHeader("Content-Type", file.endsWith("css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8"); res.end(readFileSync(join(ROOT, file))); return; }
    if (req.url === "/") { res.setHeader("Content-Type", "text/html"); res.end(html); return; }
    res.writeHead(404); res.end();
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  browser = await chromium.launch(); page = await browser.newPage({viewport:{width:1440, height:900}});
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => !!window.TrellisPeleSequence && !!window.pele);
});
test.after(async () => { await browser?.close(); await new Promise(done => server?.close(done)); });

const LONG = "a_condition_long_enough_that_its_row_would_widen_every_column_of_the_diagram(with, many, arguments, and, more, of, them)";
// The seed, `types` type columns of `methods` methods each, and a body of `steps` steps cycling
// through a call, an `if` that returns and a loop over a call.
function coordinateView({types = 7, methods = 8, steps = 150} = {}) {
  const view = baseView(), template = view.steps[0];
  const evidence = id => ({boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${id}`});
  const seedColumn = {id:"type:Coordinator", label:"Coordinator"};
  view.seed.name = "coordinate";
  view.participants = [{...view.participants[0], id:view.seed.id, label:"coordinate", column:seedColumn, declaration:view.seed}];
  for (let type = 0; type < types; type++) for (let method = 0; method < methods; method++) {
    const id = `exact:${type}:${method}`, declaration = {...structuredClone(view.seed), id, name:`method_${type}_${method}`, path:`src/t${type}.rs`};
    view.participants.push({id, label:declaration.name, kind:"method", identification:"Fresh exact target", declaration, column:{id:`type:T${type}`, label:`Type${type}`}});
  }
  const base = (id, kind, label) => ({...structuredClone(template), id, kind, label, callId:null, children:[], alternate:[], hidden:false});
  const call = n => {
    const target = view.participants[1 + n % (types * methods)];
    return {...base(`call:${n}`, "call", `self.lexical_spelling_${n}`), callId:`callid:${n}`, target:target.id, callEvidence:evidence(n)};
  };
  view.steps = [];
  for (let n = 0, shown = 0; shown < steps; n++) {
    if (n % 3 === 0) { view.steps.push(call(n)); shown++; }
    else if (n % 3 === 1) {
      const branch = base(`if:${n}`, "branch", `if ${LONG}`), exit = base(`return:${n}`, "return", `return Err(${LONG})`);
      branch.children = [exit];
      branch.folded = [base(`guard:${n}`, "branch", "only if the preceding path continues normally")];
      view.steps.push(branch); shown += 2;
    } else {
      const loop = base(`loop:${n}`, "loop", "for_expression: possible iterations, not unrolled; exit/termination unknown");
      loop.children = [call(n)];
      view.steps.push(loop); shown += 2;
    }
  }
  view.warnings = []; view.hiddenSteps = 0;
  return view;
}
async function render(view, options = {}, id = "sequence-diagram") {
  return page.evaluate(async ({view, options, id}) => {
    const container = document.getElementById(id);
    container.removeAttribute("data-fit");
    window.picks = []; window.opens = [];
    const result = await TrellisPeleSequence.render(container, view, null, new Set(), {...options, onSelect:step => window.picks.push(step), onOpenTarget:declaration => window.opens.push(declaration.id)});
    return {status:result.status, reason:result.reason};
  }, {view, options, id});
}
const geometry = id => page.evaluate(id => {
  const box = document.getElementById(id), svg = box.querySelector("svg"), view = svg.viewBox.baseVal;
  const scale = svg.getScreenCTM().a;
  const sizes = [...svg.querySelectorAll("text")].filter(node => node.textContent.trim()).map(node => parseFloat(getComputedStyle(node).fontSize) * scale);
  return {scale, smallest:Math.min(...sizes), viewWidth:view.width, viewHeight:view.height, width:svg.getBoundingClientRect().width, clientWidth:box.clientWidth, scrollWidth:box.scrollWidth, clientHeight:box.clientHeight, scrollHeight:box.scrollHeight};
}, id);

test("a coordinate-shaped diagram draws one lane per type with the method on each arrow", async () => {
  const view = coordinateView();
  const result = await render(view);
  assert.equal(result.status, "rendered", result.reason);
  const actors = await page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").allTextContents();
  assert.deepEqual([...actors].sort(), ["Coordinator", "Type0", "Type1", "Type2", "Type3", "Type4", "Type5", "Type6"]);
  const call = view.steps.find(step => step.kind === "call"), target = view.participants.find(p => p.id === call.target);
  const label = page.locator(`#sequence-diagram .pele-edge-label[data-trellis-step-id="${call.id}"]`);
  assert.equal(await label.textContent(), `[001] ${target.declaration.name}`);
  // The arrow carries the exact target, and a click hands the inspector the original step.
  assert.equal(await page.locator(`#sequence-diagram .pele-message[data-trellis-step-id="${call.id}"]`).getAttribute("data-trellis-target-id"), call.target);
  await label.click();
  const picked = await page.evaluate(() => window.picks[0]);
  assert.deepEqual(picked, call, "the step, its source label and its evidence are unchanged");
  assert.match(await label.getAttribute("aria-label"), /lexical_spelling_0/);
});

test("the 21st method of a type costs no lane; the 21st type still does", async () => {
  const many = coordinateView({types:1, methods:30, steps:60});
  assert.equal((await render(many)).status, "rendered");
  assert.equal(await page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").count(), 2);
  const wide = coordinateView({types:20, methods:1, steps:150});
  const refused = await render(wide);
  assert.equal(refused.status, "unsupported");
  assert.match(refused.reason, /Too many sequence participants/);
});

test("a lane of several methods opens none; a lane of one method keeps its action", async () => {
  const view = coordinateView({types:2, methods:2, steps:12});
  view.participants.splice(4, 1);
  for (const step of view.steps.flatMap(step => [step, ...step.children])) if (step.target === "exact:1:1") step.target = "exact:1:0";
  assert.equal((await render(view)).status, "rendered");
  const several = await page.evaluate(() => [...document.querySelectorAll(".pele-actor[data-trellis-target-id]")].map(node => node.getAttribute("data-trellis-target-id")));
  assert.deepEqual([...new Set(several)], ["exact:1:0"], "only Type1 has one target");
  await page.locator('.pele-actor[data-trellis-target-id="exact:1:0"]').first().click();
  assert.deepEqual(await page.evaluate(() => window.opens), ["exact:1:0"]);
});

test("the legacy renderer draws the same columns", async () => {
  const view = coordinateView({types:2, methods:3, steps:20});
  const positions = await page.evaluate(view => {
    const box = document.getElementById("sequence-diagram");
    TrellisSequence.render(box, view, null, new Set(), {});
    const ends = [...box.querySelectorAll(".sequence-arrow[data-trellis-target-id]")].map(arrow => [arrow.getAttribute("data-trellis-target-id"), arrow.getAttribute("d")]);
    return {labels:[...box.querySelectorAll(".sequence-participant-label")].map(node => node.firstChild.textContent), ends};
  }, view);
  assert.deepEqual(positions.labels, ["Coordinator", "Type0", "Type1"]);
  const end = id => positions.ends.find(([target]) => target === id)[1];
  const x = id => end(id).match(/H (\d+)/)[1];
  assert.equal(x("exact:0:0"), x("exact:0:2"), "two methods of one type end on one lifeline");
  assert.notEqual(x("exact:0:0"), x("exact:1:0"));
});

test("a large diagram opens at readable size and the panel scrolls", async () => {
  assert.equal((await render(coordinateView())).status, "rendered");
  const open = await geometry("sequence-diagram");
  assert.ok(Math.abs(open.scale - 1) < 0.001, `drawn at its own size, not shrunk: ${JSON.stringify(open)}`);
  assert.ok(open.smallest >= 12, `smallest text ${open.smallest}px`);
  assert.ok(open.scrollHeight > open.clientHeight, `the panel scrolls down the diagram: ${JSON.stringify(open)}`);
  assert.ok(open.scrollWidth > open.clientWidth, "and across it");
  // Row labels are bounded: a long condition does not set the width of the diagram.
  assert.ok(open.viewWidth < 4000, `width ${open.viewWidth}`);
  const moved = await page.evaluate(() => { const box = document.getElementById("sequence-diagram"); box.scrollTop = 600; box.scrollLeft = 200; return [box.scrollTop, box.scrollLeft]; });
  assert.deepEqual(moved, [600, 200]);
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true, "the page itself never scrolls sideways");
});

test("fit shrinks to the panel on request, and the Files preview keeps fitting", async () => {
  assert.equal((await render(coordinateView())).status, "rendered");
  await page.evaluate(() => document.getElementById("sequence-diagram").setAttribute("data-fit", ""));
  const fitted = await geometry("sequence-diagram");
  assert.ok(fitted.width <= fitted.clientWidth, JSON.stringify(fitted));
  assert.ok(fitted.scale < 1);
  assert.equal((await render(coordinateView({steps:30}), {}, "files-mini-sequence")).status, "rendered");
  const mini = await geometry("files-mini-sequence");
  assert.ok(mini.width <= mini.clientWidth, JSON.stringify(mini));
  await page.evaluate(() => document.getElementById("files-mini-sequence").replaceChildren());
});

test("the full labels stay in details mode", async () => {
  const view = coordinateView({steps:9});
  assert.equal((await render(view, {showDetails:true})).status, "rendered");
  const branch = view.steps.find(step => step.kind === "branch");
  const note = await page.locator(`#sequence-diagram .pele-note[data-trellis-step-id="${branch.id}"]`).textContent();
  assert.ok(note.includes(LONG), note);
  assert.match(note, /may exit early/);
});

test("a branch that folds its guard is marked at its own row and nowhere else", async () => {
  const view = coordinateView({steps:9});
  assert.equal((await render(view)).status, "rendered");
  const texts = await page.locator("#sequence-diagram .pele-note[data-trellis-step-id]").evaluateAll(nodes => nodes.map(node => [node.dataset.trellisStepId, node.textContent]));
  for (const [id, text] of texts) assert.equal(/may exit early/.test(text), id.startsWith("if:"), `${id}: ${text}`);
});

test("the empty state names what is hidden", async () => {
  const summary = view => page.evaluate(view => TrellisSequence.emptySummary(view), view);
  const hidden = (incidental, name) => ({id:name, kind:"call", hidden:true, incidental, children:[], alternate:[], callEvidence:{boundaryReason:"external", externalPackage:name ? {manager:"cargo", name, version:"1"} : undefined}});
  assert.equal(await summary({steps:[hidden("external", "std"), hidden("external", "core")], hiddenSteps:2}), "Only standard-library calls (2 hidden).");
  assert.equal(await summary({steps:[hidden("external", "std"), hidden("external", "serde")], hiddenSteps:2}), "Only external calls (2 hidden).");
  assert.equal(await summary({steps:[hidden("external", "std"), hidden("logging")], hiddenSteps:2}), "Only incidental calls (2 hidden).");
  assert.equal(await summary({steps:[hidden("external", "std"), {...hidden("external", "std"), hidden:false}], hiddenSteps:1}), "", "a shown step draws the diagram");
  assert.equal(await summary({steps:[], hiddenSteps:0}), "", "no steps at all has its own message");
  await page.evaluate(() => {
    window.shown = 0;
    TrellisSequence.renderEmpty(document.getElementById("sequence-diagram"), "Only standard-library calls (2 hidden).", () => window.shown++);
  });
  assert.equal(await page.locator("#sequence-diagram svg").count(), 0, "no lone box");
  assert.equal(await page.locator("#sequence-diagram .sequence-empty").textContent(), "Only standard-library calls (2 hidden). Show them");
  await page.locator("#sequence-diagram .sequence-empty button").click();
  assert.equal(await page.evaluate(() => window.shown), 1);
});
