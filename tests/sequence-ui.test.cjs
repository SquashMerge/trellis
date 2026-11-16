"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
// web/sequence.js after the script it reads the source witness from, as index.html loads them.
// Then web/pele-svg.js, which web/sequence-pele.js reads its shared helpers from.
const sequenceSource = ["web/source-witness.js", "web/sequence-model.js", "web/sequence.js", "web/pele-svg.js"].map(file => fs.readFileSync(file, "utf8")).join("\n");
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.textContent = ""; this.listeners = {}; }
  setAttribute(key, value) { this.attrs[key] = value; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  addEventListener(name, handler) { this.listeners[name] = handler; }
}
// web/sequence.js with a document that only makes elements: the model and the two plain states.
function sequenceModel() {
  const window = {};
  vm.runInNewContext(sequenceSource, {window, document:{createElement:tag => new Element(tag)}});
  return window.TrellisSequence;
}
const range = {startLine:3, startColumn:2, endLine:7, endColumn:9};
const call = (id, target = "type", resolution = "unresolved") => ({id, kind:"call", label:id, path:"main.rs", range, callId:id, target, resolution, children:[], alternate:[]});
const viewWith = steps => ({seed:{id:"seed", name:"sample"}, participants:[
  {id:"seed", label:"sample", kind:"method"},
  {id:"type", label:"Builder", kind:"externalCandidate", identification:"Syntax candidate; not proven receiver identity"},
  {id:"internal", label:"helper", kind:"internal"},
  {id:"unknown", label:"Unknown", kind:"unresolvedReceiver"}
], steps});
const freeze = value => { if (value && typeof value === "object") { Object.freeze(value); Object.values(value).forEach(freeze); } return value; };

// #1007: web/sequence.js draws no diagram. When Pele cannot draw, the caller shows this state.
test("a Sequence that could not be drawn is one plain line with the reason as text, and nothing else", () => {
  const sequence = sequenceModel(), container = new Element("div");
  assert.equal(sequence.render, undefined, "there is no second renderer to fall back to");
  container.append(new Element("svg"));
  sequence.renderError(container, "Unsupported sequence depth");
  assert.equal(container.children.length, 1, "the previous diagram is replaced");
  const [note] = container.children;
  assert.equal(note.tag, "p"); assert.equal(note.className, "sequence-empty", "the empty state's own styling");
  assert.equal(note.textContent, "This Sequence could not be drawn: Unsupported sequence depth");
  assert.deepEqual(note.children, [], "no control, no partial diagram");
  sequence.renderError(container, '<img src=x onerror="boom()">');
  assert.equal(container.children.length, 1);
  assert.equal(container.children[0].textContent, 'This Sequence could not be drawn: <img src=x onerror="boom()">', "repository text stays text");
  assert.deepEqual(container.children[0].children, []);
});
test("the error state takes the container's render ownership, so an older render cannot install over it", () => {
  const sequence = sequenceModel(), container = new Element("div"), other = new Element("div");
  const older = sequence.beginRender(container), elsewhere = sequence.beginRender(other);
  assert.equal(older(), true);
  sequence.renderError(container, "Pele asset unavailable");
  assert.equal(older(), false, "the render it replaced is no longer current");
  assert.equal(elsewhere(), true, "another container's render is untouched");
  const newer = sequence.beginRender(container);
  assert.equal(newer(), true, "a later render owns the container again");
});

// The compact text both the diagram rows and the inspector start from. The full label stays on
// the original step.
test("compact labels shorten known parser prose and leave unknown text as measured", () => {
  const sequence = sequenceModel();
  const step = (kind, label) => freeze({id:"step", kind, label, path:"main.rs", range, children:[], alternate:[]});
  for (const [kind, label, compact] of [
    ["boundary", "Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.", "nested definition · effects not expanded"],
    ["boundary", "Argument unpacking boundary: iteration/mapping protocol not expanded.", "argument unpacking · protocol not expanded"],
    ["boundary", "Formatted string boundary: interpolation and formatting protocol not expanded.", "formatted string · protocol not expanded"],
    ["boundary", "Unsupported custom behavior: evaluation remains unknown.", "Unsupported custom behavior: evaluation remains unknown."],
    ["loop", "for_expression: possible iterations, not unrolled; exit/termination unknown", "for · possible iterations"],
    ["loop", "while_expression: possible iterations, not unrolled; exit/termination unknown", "while · possible iterations"],
    ["loop", "loop_expression: possible iterations, not unrolled; exit/termination unknown", "loop · termination unknown"],
    ["branch", "?: continue only on success; otherwise early return", "success / early return"],
    ["effect", "write: obj.value = prepare() (implicit setters/unpacking unresolved)", "write · setters/unpacking unresolved"],
    ["branch", "total > 0 <unsafe>", "total > 0 <unsafe>"],
  ]) assert.equal(sequence.compactLabel(step(kind, label)), compact, label);
});

// Pele projection tests use the real adapter, not a reconstructed prototype DTO.
function peleProjector() {
  const window = {};
  const context = vm.createContext({window, TextEncoder});
  vm.runInContext(sequenceSource, context);
  vm.runInContext(fs.readFileSync("web/sequence-pele.js", "utf8"), context);
  return window.TrellisPeleSequence;
}
function peleFixture(language) {
  return JSON.parse(fs.readFileSync(`tests/fixtures/sequence-pele/${language}.json`, "utf8"));
}
test("four measured language DTOs project to one lane, ordered source notes and no invented messages", () => {
  const adapter = peleProjector();
  for (const language of ["javascript", "rust", "java", "python"]) {
    const view = peleFixture(language);
    const result = adapter.project(view);
    assert.match(result.source, /^%%\{init: [^\n]*\}%%\nsequenceDiagram\nparticipant S as /);
    assert.equal((result.source.match(/^participant /gm) || []).length, 1);
    assert.equal((result.source.match(/^Note right of S:/gm) || []).length, result.rows.length);
    assert.doesNotMatch(result.source, /S\s*[-=]+>/);
    assert.equal(new Set(result.rows.map(row => row.id)).size, result.rows.length);
    for (const row of result.rows) {
      assert.equal(row.step.id, row.id);
      assert.equal(row.step.path, row.path);
      assert.strictEqual(row.step.range, row.range);
      assert.strictEqual(row.revision, view.revision);
      assert.ok(row.range.startLine > 0);
    }
    assert.ok(result.rows.some(row => row.callId), language);
  }
});
test("Pele projection rejects duplicates and encodes malicious repository text", () => {
  const adapter = peleProjector(), view = peleFixture("rust");
  view.seed.name = '<script onload="evil()">';
  view.steps[0].label = 'unsafe\nalt <img src=x>';
  const result = adapter.project(view);
  assert.doesNotMatch(result.source, /<script|<img|\nalt </);
  view.steps.push(view.steps[0]);
  assert.throws(() => adapter.project(view), /Duplicate sequence identity/);
});

test("Pele projection keeps human labels separate from encoded Mermaid and refuses unsupported target evidence", () => {
  const adapter = peleProjector(), view = peleFixture("rust");
  view.steps[0].label = 'if check() Vec<T> & "quoted" / café 🔒';
  const result = adapter.project(view,new Set(),{showDetails:true});
  assert.ok(result.rows[0].display.includes(view.steps[0].label));
  assert.match(result.source, /#40;#41;/);
  assert.match(result.source, /#60;T#62;/);
  assert.match(result.source, /#233;/);
  assert.doesNotMatch(result.source, /U\+[0-9A-F]+|Vec<T>/);
  const cases = [
    step => step.target = view.participants[0].id,
    step => step.target = "dangling-id",
    step => {step.hidden=true;step.target="dangling-id";},
    step => {step.children=[{...step,id:"nested-target",target:"dangling-id",children:[],alternate:[]}];},
    step => {step.kind="group";step.children=[{...step,id:"collapsed-target",kind:"call",target:"dangling-id",children:[],alternate:[]}];}
  ];
  for (const change of cases) {
    const candidate=peleFixture("rust");candidate.steps=[structuredClone(candidate.steps[0])];
    change(candidate.steps[0]);
    assert.throws(()=>adapter.project(candidate),/Invalid resolved target evidence/);
  }
});

test("frozen real Pele 1.10.0-trellis.2 sequence and grammar output remain byte-identical", () => {
  const sandbox = {};
  vm.runInNewContext(fs.readFileSync("web/vendor/pele-1.10.0-trellis.2.js", "utf8"), sandbox);
  assert.equal(typeof sandbox.pele.render, "function");
  for (const name of ["sequence", "representative"]) {
    const source = fs.readFileSync(`tests/fixtures/pele-1.10.0-trellis.2/${name}.mmd`, "utf8");
    const output = sandbox.pele.render(source).svg;
    assert.equal(output, fs.readFileSync(`tests/fixtures/pele-1.10.0-trellis.2/${name}.svg`, "utf8"));
  }
  const actual = fs.readFileSync("tests/fixtures/pele-1.10.0-trellis.2/sequence.svg", "utf8");
  assert.deepEqual([...actual.matchAll(/class="pele-note" data-id="(i\d+)"/g)].map(match => match[1]),
    ["i0", "i1", "i2", "i3", "i4", "i5", "i6", "i8", "i10"]);
  assert.match(actual, /class="pele-frame pele-frame-alt"/);
  assert.doesNotMatch(actual, /class="pele-frame pele-frame-alt" data-id=/);
});

test("negative control: original step identity check detects altered projector", () => {
  const source = fs.readFileSync("web/sequence-pele.js", "utf8");
  const fixture = peleFixture("rust");
  const evaluate = text => {
    const window = {};
    const context = vm.createContext({window, TextEncoder});
    vm.runInContext(sequenceSource, context);
    vm.runInContext(text, context);
    const row = window.TrellisPeleSequence.project(fixture).rows[0];
    assert.equal(row.id, row.step.id, "Pele map must retain original measured identity");
  };
  evaluate(source); // positive production behavior
  const mutated = source.replace("id:step.id, callId:step.callId", 'id:"mutated", callId:step.callId');
  assert.notEqual(mutated, source);
  assert.throws(() => evaluate(mutated), /Pele map must retain original measured identity/);
  evaluate(source); // restored production behavior
});

test("pinned Pele renders all four producer Mermaid sources and bounded near-cap input", () => {
  const sandbox = {};
  vm.runInNewContext(fs.readFileSync("web/vendor/pele-1.10.0-trellis.2.js", "utf8"), sandbox);
  const adapter = peleProjector();
  for (const language of ["javascript", "rust", "java", "python"]) {
    const projection = adapter.project(peleFixture(language));
    const result = sandbox.pele.render(projection.source).svg;
    assert.match(result, /^<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" class="pele pele-sequence"/);
    assert.equal((result.match(/class="pele-note" data-id="i\d+"/g) || []).length,
      projection.rows.length + projection.separators, language);
    assert.equal((result.match(/class="pele-frame pele-frame-/g) || []).length,
      projection.frames.filter(frame => frame !== "rect").length, language);
    assert.equal((result.match(/class="pele-rect"/g) || []).length,
      projection.frames.filter(frame => frame === "rect").length, language);
  }
  const near = peleFixture("rust");
  near.steps = Array.from({length: 180}, (_, i) => ({...near.steps[0], id:`near-${i}`}));
  const projection = adapter.project(near);
  assert.equal(projection.rows.length, 180);
  assert.equal((sandbox.pele.render(projection.source).svg.match(/class="pele-note" data-id=/g) || []).length, 181);
  near.steps = Array.from({length: 201}, (_, i) => ({...near.steps[0], id:`overflow-${i}`}));
  assert.throws(() => adapter.project(near), /Too many source notes/);
});

test("Pele rejects changed inline style and oversized SVG before DOMParser", async () => {
  const source = fs.readFileSync("web/sequence-pele.js", "utf8");
  const frozen = fs.readFileSync("tests/fixtures/pele-1.10.0-trellis.2/sequence.svg", "utf8");
  const view = peleFixture("rust");
  let parses = 0;
  const sandbox = {window:{pele:{render:() => ({svg:frozen.replace("max-width:100%", "max-width:200%")})}},
    TextEncoder, DOMParser:class { parseFromString() { parses++; throw Error("should not parse"); } }};
  vm.runInNewContext(sequenceSource, sandbox);
  vm.runInNewContext(source, sandbox);
  const container = {replaceChildren() { throw Error("partial DOM installed"); }};
  const changed = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(changed.status, "unsupported");
  assert.match(changed.reason, /theme/);
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => ({svg:"x".repeat(8 * 1024 * 1024 + 1)});
  const oversized = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(oversized.status, "unsupported");
  assert.match(oversized.reason, /too large/);
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => { throw Error("render failed"); };
  const failed = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(failed.status, "unsupported");
  assert.equal(failed.reason, "render failed");
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => ({svg:frozen});
  const stale = await sandbox.window.TrellisPeleSequence.render(container, view, null, new Set(), {isCurrent:() => false});
  assert.equal(stale.status, "stale");
});

// #1018: Sequence rejects active or resource markup before DOMParser, as the other Pele adapters
// do. The element allowlist rejected these elements already, but only after parsing them.
test("Pele rejects active and resource markup before DOMParser, and still parses what Pele draws", () => {
  const vendor = {};
  vm.runInNewContext(fs.readFileSync("web/vendor/pele-1.10.0-trellis.2.js", "utf8"), vendor);
  let parses = 0;
  const sandbox = {window:{}, TextEncoder, DOMParser:class { parseFromString() { parses++; throw Error("parsed"); } }};
  vm.runInNewContext(sequenceSource, sandbox);
  vm.runInNewContext(fs.readFileSync("web/sequence-pele.js", "utf8"), sandbox);
  const adapter = sandbox.window.TrellisPeleSequence;
  for (const language of ["javascript", "rust", "java", "python"]) {
    const projection = adapter.project(peleFixture(language)), raw = vendor.pele.render(projection.source, {}).svg;
    for (const markup of ["<script>1</script>", "<foreignObject></foreignObject>", '<image href="x"/>', '<a href="x"></a>', "</script>", "<!-- <script -->"]) {
      parses = 0;
      assert.throws(() => adapter.validatedSvg(raw.replace("</svg>", `${markup}</svg>`), projection),
        error => error.code === "unsupported" && error.message === "Unexpected SVG element", `${language} ${markup}`);
      assert.equal(parses, 0, `${language} ${markup}`);
    }
    // What Pele really draws passes the check and reaches the parser.
    parses = 0;
    assert.throws(() => adapter.validatedSvg(raw, projection), /^Error: parsed$/, language);
    assert.equal(parses, 1, language);
  }
});

test("real Chromium validates pinned SVG and source note identity under unchanged CSP", async t => {
  if (!process.env.PLAYWRIGHT_BROWSERS_PATH) { t.skip("Playwright browser is provisioned by ./tools/verify"); return; }
  const {chromium} = require("./browser/node_modules/playwright");
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("http://127.0.0.1:43987/**", route => {
      const path = new URL(route.request().url()).pathname;
      const assets = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js", "/source-witness.js":"web/source-witness.js", "/sequence-model.js":"web/sequence-model.js", "/sequence.js":"web/sequence.js", "/pele-svg.js":"web/pele-svg.js", "/sequence-pele.js":"web/sequence-pele.js"};
      if (assets[path]) return route.fulfill({status:200, contentType:"text/javascript; charset=utf-8", body:fs.readFileSync(assets[path])});
      if (path === "/test.css") return route.fulfill({status:200, contentType:"text/css", body:"#diagram { width:240px; height:110px; overflow:auto }"});
      return route.fulfill({status:200, contentType:"text/html; charset=utf-8", body:
        `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'"><script src="/vendor/pele-1.10.0-trellis.2.js"></script><script src="/source-witness.js"></script><script src="/sequence-model.js"></script><script src="/sequence.js"></script><script src="/pele-svg.js"></script><script src="/sequence-pele.js"></script><link rel="stylesheet" href="/test.css"><div id="diagram"></div>`});
    });
    await page.goto("http://127.0.0.1:43987/");
    await page.evaluate(() => document.addEventListener("securitypolicyviolation", event =>
      (window.cspViolations ||= []).push(event.violatedDirective)));
    for (const language of ["javascript", "rust", "java", "python"]) {
      const fixture = peleFixture(language);
      const actual = await page.evaluate(async view => {
        let selected;
        const result = await window.TrellisPeleSequence.render(document.querySelector("#diagram"), view,
          null, new Set(), {onSelect:step => selected=step});
        const note = document.querySelector('.pele-note[data-trellis-step-id]');
        note?.dispatchEvent(new MouseEvent("click", {bubbles:true}));
        const sameOriginalObject = selected === view.steps[0];
        selected = undefined;
        note?.dispatchEvent(new KeyboardEvent("keydown", {key:"Enter", bubbles:true}));
        const keyboardOriginalObject = selected === view.steps[0];
        selected = undefined;
        return {status:result.status, reason:result.reason, notes:document.querySelectorAll('.pele-note[data-trellis-step-id]').length,
          original:sameOriginalObject, keyboard:keyboardOriginalObject, id:note?.getAttribute("data-trellis-step-id"), csp:window.cspViolations || []};
      }, fixture);
      assert.equal(actual.status, "rendered", `${language}: ${actual.reason}`);
      assert.equal(actual.notes, 5, language);
      assert.equal(actual.original, true, language);
      assert.equal(actual.keyboard, true, language);
      assert.deepEqual(actual.csp, [], language);
    }
    // Mutate actual Pele output, then run the production validator rather than
    // merely inspecting static fixture markup. No partial DOM may be installed.
    const mutated = await page.evaluate(async view => {
      const container = document.querySelector("#diagram");
      const originalRender = window.pele.render;
      const source = window.TrellisPeleSequence.project(view).source;
      const actual = originalRender(source, {}).svg;
      const cases = {
        guard:actual.replace(/(<text class="pele-frame-label"[^>]*>)([^<]+)(<\/text>)/g,
          (whole, open, label, close) => label === "[alternate path]" ? whole : `${open}[changed guard]${close}`),
        alternate:actual.replace(/(<text class="pele-frame-label"[^>]*>)[^<]+/, "$1[changed alternate]"),
        altIdentity:actual.replace('class="pele-frame pele-frame-alt"', 'class="pele-frame pele-frame-alt" data-id="i7"'),
        positionalNotes:actual.replace(/class="pele-note" data-id="i(\d+)"/g, (_, n) => `class="pele-note" data-id="i${Number(n) + 10}"`)
      };
      const results = {};
      for (const [name, svg] of Object.entries(cases)) {
        let selected = false;
        const previous = container.firstElementChild;
        window.pele = {render:() => ({svg})};
        const outcome = await window.TrellisPeleSequence.render(container, view, null, new Set(), {onSelect:() => selected=true});
        results[name] = {status:outcome.status, reason:outcome.reason, unchanged:container.firstElementChild === previous, selected};
      }
      window.pele = {render:originalRender};
      return results;
    }, peleFixture("rust"));
    for (const [name, expected] of Object.entries({guard:/frame header/, alternate:/frame header/, altIdentity:/frame identity/, positionalNotes:/note order/})) {
      assert.equal(mutated[name].status, "unsupported", name);
      assert.match(mutated[name].reason, expected, name);
      assert.equal(mutated[name].unchanged, true, name);
      assert.equal(mutated[name].selected, false, name);
    }
    const groupView = peleFixture("rust");
    const first = groupView.steps[0], last = groupView.steps[2];
    const nested = groupView.steps[1];
    const groupLabel = "GROUP".repeat(36), childLabel = "ENTRY".repeat(36);
    groupView.steps = [
      {...first, id:"group-alpha", kind:"group", label:groupLabel, callId:null,
        children:[{...first, label:childLabel}, last], alternate:[]},
      {...nested.children[0], id:"group-beta", kind:"group", label:"second measured chain", callId:null,
        children:[nested.children[0], nested.alternate[0]], alternate:[]}
    ];
    const group = await page.evaluate(async view => {
      window.groupView = view;
      window.groupSelected = null;
      const container = document.querySelector("#diagram");
      const projection = window.TrellisPeleSequence.project(view);
      const originalLabel = view.steps[0].label, originalChildLabel = view.steps[0].children[0].label;
      const result = await window.TrellisPeleSequence.render(container, view, null, new Set(),
        {onSelect:step => window.groupSelected=step});
      const note = container.querySelector('.pele-note[data-trellis-step-id="group-alpha"]');
      const second = container.querySelector('.pele-note[data-trellis-step-id="group-beta"]');
      const toggle = note?.querySelector("button.sequence-group-control");
      const secondToggle = second?.querySelector("button.sequence-group-control");
      if (result.status !== "rendered" || !toggle || !secondToggle) return {status:result.status, reason:result.reason};
      const text = note.textContent.replace(/\s+/g, "");
      note.dispatchEvent(new MouseEvent("click", {bubbles:true}));
      const originalSelection = window.groupSelected === view.steps[0];
      window.groupSelected = null;
      container.scrollTop = 24; container.scrollLeft = 35;
      const before = [container.scrollTop, container.scrollLeft];
      toggle.click();
      // The async toggle restores focus after its render promise resolves.
      await Promise.resolve();
      for (let i=0; i<50 && container.querySelector('[data-trellis-step-id="group-alpha"] button')?.getAttribute("aria-expanded") !== "true"; i++)
        await new Promise(resolve => setTimeout(resolve, 10));
      const successor = container.querySelector('[data-trellis-step-id="group-alpha"] button');
      return {status:result.status, collapsed:2,
        longEvidence:projection.rows[0].display.includes("first measured entry: ENTRY") &&
          projection.rows[0].display.includes(view.steps[0].children[0].callId) &&
          projection.rows[0].display.includes("+1 calls collapsed (entry preview)") &&
          projection.source.includes("#43;1 calls collapsed #40;entry preview#41;"),
        visibleEvidence:text.includes("ENTRY") && text.includes("+1callscollapsed(entrypreview)") &&
          text.includes(view.steps[0].children[0].callId),
        unchanged:originalLabel === view.steps[0].label && originalChildLabel === view.steps[0].children[0].label,
        identityBound:toggle.parentElement.localName === "foreignObject" && toggle.parentElement.parentElement === note &&
          secondToggle.parentElement.parentElement === second && toggle.getAttribute("aria-controls") === note.id &&
          secondToggle.getAttribute("aria-controls") === second.id && note.getAttribute("aria-describedby") === toggle.id,
        originalSelection, noToggleSelection:window.groupSelected === null,
        previewCallId:note.getAttribute("data-trellis-preview-call-id"),
        expanded:container.querySelectorAll('.pele-note[data-trellis-step-id]').length,
        focused:document.activeElement === successor,
        scroll:[container.scrollTop, container.scrollLeft], before, csp:window.cspViolations || []};
    }, groupView);
    assert.equal(group.status, "rendered", group.reason);
    for (const key of ["longEvidence", "visibleEvidence", "unchanged", "identityBound", "originalSelection", "noToggleSelection", "focused"])
      assert.equal(group[key], true, key);
    assert.equal(group.previewCallId, first.callId);
    assert.equal(group.expanded, 4);
    assert.deepEqual(group.scroll, group.before);
    assert.deepEqual(group.csp, []);
    const secondToggle = page.locator('button.sequence-group-control[data-trellis-step-id="group-beta"]');
    await secondToggle.focus();
    const beforeEnter = await page.evaluate(() => {
      const c = document.querySelector("#diagram"); c.scrollTop = 31; c.scrollLeft = 28;
      return [c.scrollTop, c.scrollLeft];
    });
    await secondToggle.press("Enter");
    assert.deepEqual(await page.evaluate(() => {
      const c = document.querySelector("#diagram");
      return {expanded:c.querySelectorAll('.pele-note[data-trellis-step-id]').length,
        focus:document.activeElement?.dataset.trellisStepId, scroll:[c.scrollTop,c.scrollLeft],
        selected:window.groupSelected === null};
    }), {expanded:6, focus:"group-beta", scroll:beforeEnter, selected:true});
    const firstToggle = page.locator('button.sequence-group-control[data-trellis-step-id="group-alpha"]');
    await firstToggle.focus();
    await firstToggle.press("Space");
    assert.equal(await page.locator('.pele-note[data-trellis-step-id]').count(), 4);
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisStepId), "group-alpha");
    await secondToggle.focus();
    await secondToggle.press("Space");
    assert.equal(await page.locator('.pele-note[data-trellis-step-id]').count(), 2);
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisStepId), "group-beta");
    const controlView = peleFixture("rust");
    const measured = controlView.steps[0], measuredNext = controlView.steps[2];
    controlView.steps = [
      {...measured, id:"loop-1", kind:"loop", label:"possible iterations", callId:null,
        children:[{...measured, id:"loop-call"}], alternate:[]},
      {...measured, id:"try-1", kind:"try", label:"try: paths and cleanup unknown", callId:null,
        children:[{...measuredNext, id:"try-call"}], alternate:[{...measured, id:"handler-call"}]}
    ];
    const controls = await page.evaluate(async view => {
      const container = document.querySelector("#diagram");
      const result = await window.TrellisPeleSequence.render(container, view, null, new Set(), {});
      return {status:result.status, loops:container.querySelectorAll(".pele-frame-loop").length,
        rects:container.querySelectorAll(".pele-rect").length, notes:container.querySelectorAll('.pele-note[data-trellis-step-id]').length};
    }, controlView);
    assert.deepEqual(controls, {status:"rendered", loops:1, rects:1, notes:5});
    const emptyView = peleFixture("rust"); emptyView.steps = [];
    const empty = await page.evaluate(view => window.TrellisPeleSequence.render(
      document.querySelector("#diagram"), view, null, new Set(), {}), emptyView);
    assert.equal(empty.status, "rendered", empty.reason);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test("Pele projector enforces 24 visible levels and the exact 512 KiB UTF-8 Mermaid boundary", () => {
 const adapter=peleProjector(), view=peleFixture("rust"), leaf=view.steps[0];
 const chain=levels=>{let node={...leaf,id:`level-${levels-1}`,children:[],alternate:[]};
   for(let i=levels-2;i>=0;i--) node={...leaf,id:`level-${i}`,kind:"note",children:[node],alternate:[]};
   return node;};
 view.steps=[chain(24)];
 assert.equal(adapter.project(view).rows.length,24);
 view.steps=[chain(25)];assert.throws(()=>adapter.project(view),/Unsupported sequence depth/);
 view.steps=[{...leaf,id:"mermaid-bytes",label:""}];
 const sourceSize=label=>new TextEncoder().encode(adapter.project({...view,steps:[{...view.steps[0],label}]},new Set(),{showDetails:true}).source).length;
 const fixed=sourceSize("call");
 assert.ok(fixed<512*1024);
 const limit="x".repeat(512*1024-fixed+4);
 assert.equal(sourceSize(limit),512*1024);
 assert.throws(()=>adapter.project({...view,steps:[{...view.steps[0],label:limit+"x"}]},new Set(),{showDetails:true}),/Mermaid source too large/);
});

function resolvedSequence() {
  const view = peleFixture("rust"), range = {startLine:2,endLine:4};
  const target = {id:"actual-declaration", name:"actual_target", kind:"function", path:"target.rs", range};
  view.participants.push({id:target.id, label:target.name, kind:"internal", declaration:target});
  const call = {...view.steps[0], id:"original-call", callId:"original-occurrence", label:"lexical_decoy()", target:target.id,
    hidden:false, children:[], alternate:[], callEvidence:{boundaryReason:"none",resolution:"resolved",dispatch:"direct",freshness:"fresh",provenanceId:"proof"}};
  view.steps = [call];
  return {view, call, target};
}
test("resolved projection maps original occurrences and participants, not call spelling or statement offsets", () => {
  const adapter = peleProjector(), {view, call, target} = resolvedSequence();
  const repeat = {...call,id:"repeat",callId:"second-occurrence"};
  const branch = {...call,id:"guard",kind:"branch",callId:null,target:null,callEvidence:null,label:"condition",children:[call],alternate:[repeat]};
  view.steps = [branch];
  const result = adapter.project(view);
  assert.equal(result.lanes.length,2);
  assert.equal(result.lanes[1].targets.get(target.id).participant.declaration,target);
  assert.match(result.source,/participant P1 as actual_<br\/>target\n/);
  assert.equal(result.rows[1].step,call);
  assert.equal(result.rows[2].step,repeat);
  assert.equal(result.rows[1].statementId,"i3");
  assert.equal(result.rows[2].statementId,"i5");
  assert.equal(result.messageIds.join("|"),"i3|i5");
  assert.equal(result.noteIds.join("|"),"i0|i2");
  assert.equal(result.closeFrames.join("|"),"alt");
  assert.equal(result.rows[1].target.declaration.id,target.id);
});
test("collapsed sequence uses only first entry target and expands exact targets without ghost lanes", () => {
  const adapter = peleProjector(), {view,call,target} = resolvedSequence();
  const other = {...target,id:"other-declaration",name:"other"};
  view.participants.push({id:other.id,label:other.name,kind:"internal",declaration:other});
  const second = {...call,id:"second",callId:"second-occurrence",target:other.id};
  const hidden = {...second,id:"hidden",callId:"hidden-occurrence",hidden:true};
  const group = {...call,id:"chain",kind:"group",callId:null,target:null,callEvidence:null,children:[call,second]};
  view.steps = [group,hidden];
  const folded = adapter.project(view);
  assert.equal(folded.lanes.map(l=>l.id).join("|"),`${view.seed.id}|${target.id}`);
  assert.equal(folded.rows[0].preview,call);
  assert.equal(folded.rows[0].target.declaration,target);
  const open = adapter.project(view,new Set([group.id]));
  assert.equal(open.lanes.length,3);
  assert.equal(open.rows.filter(row=>row.type==="message").length,2);
});
test("nonresolved engine boundaries keep honest labels and never gain target lanes", () => {
  const adapter = peleProjector(), {view,call} = resolvedSequence();
  for (const boundaryReason of ["unresolved","ambiguous","stale","dispatch","external","missingEvidence","nodeLimit"]) {
    view.steps = [{...call,target:null,callEvidence:{boundaryReason}}];
    const result = adapter.project(view);
    assert.equal(result.lanes.length,1);
    assert.equal(result.rows[0].type,"note");
    assert.match(result.rows[0].display,/Unresolved|Ambiguous|Stale|Dispatch|External|Unbound|participant limit/);
  }
});
test("long repository call labels cannot hide nonresolved state words", () => {
  const adapter = peleProjector(), {view,call} = resolvedSequence();
  for (const boundaryReason of ["unresolved","ambiguous","stale"]) {
    const step = {...call,label:"long_expression_".repeat(80),target:null,callEvidence:{boundaryReason}};
    view.steps = [step];
    const label = boundaryReason[0].toUpperCase()+boundaryReason.slice(1);
    assert.match(adapter.project(view).rows[0].display,new RegExp(label));
  }
});

test("measured boundary calls retain their control-flow context and exact engine target", () => {
  const {view,call,target} = resolvedSequence();
  const boundary = {...call,kind:"boundary",label:"Optional-chain call: conditional argument evaluation"};
  view.steps = [boundary];
  const projection = peleProjector().project(view), row = projection.rows[0];
  assert.equal(row.step,boundary); assert.equal(row.target.declaration,target); assert.equal(row.type,"note");
  assert.match(row.display,/Resolved declaration - Optional-chain/);
  assert.equal(projection.lanes[1].targets.get(target.id).participant.declaration,target,"the target keeps its lane");
  assert.equal(projection.messageIds.length,0,"a boundary draws no arrow");
});

test("resolved noncallable declaration keeps its lane and reads source, not a callee sequence", () => {
  const {view,target} = resolvedSequence(), sequence = sequenceModel();
  assert.equal(sequence.targetActionLabel(target, view.participants.at(-1)),"Open callee sequence");
  target.kind = "class";
  assert.equal(sequence.targetActionLabel(target, view.participants.at(-1)),"Read declaration source");
  assert.equal(peleProjector().project(view).lanes.length,2);
});

// #522: a resolved call's expanded body is drawn on the callee's lane.
test("a resolved call's expanded body is drawn on the callee lane", () => {
  const {view, call, target} = resolvedSequence();
  const inner = {id:"inner-declaration", name:"helper", kind:"function", path:"target.rs", range:{startLine:5,endLine:5}};
  view.participants.push({id:inner.id, label:inner.name, kind:"function", declaration:inner});
  const child = {...call, id:"original-call>step:1:1", callId:"inner-occurrence", label:"helper()", path:"target.rs", lane:target.id, target:inner.id, hidden:false, children:[], alternate:[]};
  call.children = [child];
  const projection = peleProjector().project(view);
  assert.ok(projection.rows.some(row => row.id === child.id && row.type === "message"));
  assert.match(projection.source, /^P1->>P2: /m);
});

// #546 review: a collapsed chain whose last call may return early (#542) keeps the marker in
// its compact row.
test("collapsed chain retains the early-return marker in its compact row", () => {
  const measured = id => ({...call(id), target:null, hidden:false});
  const second = {...measured("open"), folded:[{id:"try", kind:"branch", label:"?: continue only on success; otherwise early return", path:"main.rs", range,
    alternate:[{id:"return", kind:"return", label:"?: early return on residual", path:"main.rs", range}]}]};
  const group = {id:"chain", kind:"group", label:"Builder chain", path:"main.rs", range, children:[measured("Builder::new"), second], alternate:[], hidden:false};
  const view = {...viewWith([group]), revision:{indexGeneration:"pin", indexRevision:1}};
  const [row] = peleProjector().project(view).rows;
  assert.equal(row.step, group);
  assert.match(row.display, /calls collapsed \(entry preview\) · may return early$/);
});

// Normal-size admitted source tree: the display must not rewrite this evidence.
function callsFirstView() {
  const leaf = (id, kind, label = id, extra = {}) => ({id, kind, label, path:"main.rs", range, children:[], alternate:[], hidden:false, ...extra});
  const measured = id => leaf(id, "call", id, {callId:id});
  return {...viewWith([
    measured("prepare"), leaf("write", "effect", "write: state = ready"),
    leaf("definition", "definition", "define callback · body deferred"),
    leaf("nested", "boundary", "Nested callable boundary: body does not execute at definition; captures are not expanded."),
    leaf("condition", "branch", "if ready", {children:[measured("ready"),leaf("return", "return", "return result")], alternate:[measured("fallback"),leaf("write-alt", "effect", "write: fallback")] }),
    leaf("loop", "loop", "loop_expression: possible iterations, not unrolled; exit/termination unknown", {children:[measured("repeat"),leaf("write-loop", "effect", "write: iteration")] }),
    leaf("unknown", "boundary", "Unsupported complex control: possible behavior remains unknown; body not expanded."),
    leaf("continuation", "branch", "only if the preceding path continues normally", {children:[measured("finish")] }),
    leaf("last-return", "return", "return final_value")
  ]), revision:{indexGeneration:"12345678-1234-4123-8123-123456789abc", indexRevision:1}};
}
test("calls-first keeps original ordered calls, conditional/unknown context and reversible detail", () => {
  const view = freeze(callsFirstView()), before = JSON.stringify(view), adapter = peleProjector();
  const projection = adapter.project(view);
  const retained = ["prepare","condition","ready","fallback","loop","repeat","unknown","continuation","finish"];
  assert.deepEqual(Array.from(projection.rows,row=>row.id),retained);
  const originals = view.steps.flatMap(function walk(step) {return [step,...(step.children||[]).flatMap(walk),...(step.alternate||[]).flatMap(walk)];});
  assert.equal(originals.length,16);
  for (const row of projection.rows) {
    const original = originals.find(step=>step.id===row.id);
    assert.equal(row.step,original); assert.equal(row.range,original.range);
  }
  assert.match(projection.rows.find(row=>row.id==="condition").display,/may exit early/);
  assert.match(projection.source,/alt if ready/); assert.match(projection.source,/else alternate path/);
  assert.match(projection.source,/loop loop/); assert.match(projection.source,/unknown/);
  const all = adapter.project(view,new Set(),{showDetails:true});
  assert.deepEqual(Array.from(all.rows,row=>row.id),originals.map(step=>step.id));
  assert.match(all.rows.find(row=>row.id==="write").display,/write: state = ready/);
  assert.equal(JSON.stringify(view),before);
});
test("calls-first zero-call summary never claims that a method has no behavior", () => {
  const window = {};
  vm.runInNewContext(sequenceSource,{window});
  const view = viewWith([{id:"write",kind:"effect",label:"write state",children:[],alternate:[]}]);
  assert.match(window.TrellisSequence.emptySummary(view),/No measured calls.*non-call behavior.*Show all steps/i);
  assert.equal(window.TrellisSequence.emptySummary(view,{showDetails:true}),"");
});

test("calls-first hides deferred callables but keeps unexpanded initialization effects", () => {
  const labels = [
    "Nested callable boundary: body does not execute at definition; captures are not expanded.",
    "Lambda boundary: body does not execute at definition; captures are not expanded.",
    "Nested declaration boundary: bodies are not executed here; implicit initialization is not expanded.",
    "Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded."
  ];
  const steps = labels.map((label,i)=>({id:`deferred-${i}`,kind:"boundary",label,path:"main.rs",range,children:[],alternate:[],hidden:false}));
  const view = freeze({...viewWith(steps),revision:{indexGeneration:"pin",indexRevision:1}}), adapter = peleProjector();
  assert.deepEqual(Array.from(adapter.project(view).rows,row=>row.id),["deferred-2","deferred-3"]);
  assert.deepEqual(Array.from(adapter.project(view,new Set(),{showDetails:true}).rows,row=>row.id),steps.map(step=>step.id));
});

test("calls-first preserves extractor-labelled uncertainty outside boundaries only", () => {
  const make=(id,kind,label)=>({id,kind,label,path:"main.py",range,children:[],alternate:[],hidden:false});
  const ordinary=[make("bind","effect","bind name: unknown"),make("write","effect","write: unknown = unresolved"),make("ordinary-loop","loop","for_statement: possible iterations, not unrolled")];
  const uncertain=[
    make("setter","effect","write: obj.value = prepare() (implicit setters/unpacking unresolved)"),
    make("iteration","effect","iteration target write: implicit setters/unpacking unresolved"),
    make("python-loop","loop","possible iterations, not unrolled; protocol and termination unresolved"),
    make("java-iter","loop","enhanced for: possible iterations; iterable evaluated once; implicit iterator/array protocol not expanded"),
    make("java-loop","loop","loop: possible iterations, not unrolled; termination and transfers unknown"),
    ...["for_expression","while_expression","loop_expression"].map(kind=>make(kind,"loop",`${kind}: possible iterations, not unrolled; exit/termination unknown`)),
    make("js-try","try","try; exception behavior unknown")
  ];
  const view=freeze({...viewWith([...ordinary,...uncertain]),revision:{indexGeneration:"pin",indexRevision:1}}),before=JSON.stringify(view),adapter=peleProjector();
  const compact=adapter.project(view);
  assert.deepEqual(Array.from(compact.rows,row=>row.id),uncertain.map(step=>step.id));
  assert.match(compact.rows.find(row=>row.id==="setter").display,/write · setters\/unpacking unresolved/);
  assert.deepEqual(Array.from(adapter.project(view,new Set(),{showDetails:true}).rows,row=>row.id),view.steps.map(step=>step.id));
  assert.equal(JSON.stringify(view),before);
});


test("older-basis sequence evidence keeps historical identity and is not labelled fresh", () => {
  const window = {};
  vm.runInNewContext(sequenceSource, {window});
  const projection = {semanticStatus:"olderBasis", currentPin:{indexGeneration:"generation",indexRevision:2},
    semanticBasisPin:{indexGeneration:"generation",indexRevision:1},basisNativeRevisionId:"native-P"};
  const evidence = {projection, boundaryReason:"none",freshness:"possiblyStale",historicalFreshness:"fresh"};
  assert.match(window.TrellisSequence.evidenceLabel({kind:"call",callId:"call",callEvidence:evidence}), /older basis/);
  assert.match(window.TrellisSequence.basisLabel(evidence), /generation:1.*native-P/);
  assert.equal(window.TrellisSequence.evidenceSummary({semantic:{documents:[{projection}]}}), "Older semantic basis");
  projection.semanticStatus = "catchingUp";
  assert.match(window.TrellisSequence.evidenceLabel({kind:"call",callId:"call",callEvidence:evidence}), /catching up/);
  assert.equal(window.TrellisSequence.evidenceSummary({semantic:{documents:[{projection}]}}), "Semantic catching up");
});

// #663: Interactions bound the arrows they draw, after internal steps are filtered out. Internal
// steps never use that room, and omitted arrows are counted as interactions, not internal steps.
test("Interactions cap arrows after filtering internal steps and count the omitted ones", () => {
  const window = {};
  vm.runInNewContext(sequenceSource, {window});
  const witness = {path:"a.rs", range:{startLine:1, endLine:1}};
  const seed = {id:"seed", name:"run", ...witness};
  const participants = [{id:"seed", label:"run", declaration:seed, column:{id:"A", label:"A"}},
    {id:"send", label:"send", declaration:{id:"send", name:"send", ...witness}, column:{id:"B", label:"B"}}];
  const call = id => ({id, kind:"call", label:"B::send", callId:`measured-${id}`, target:"send", ...witness, hidden:false,
    callEvidence:{boundaryReason:"none", resolution:"resolved", freshness:"fresh", dispatch:"direct", provenanceId:`proof-${id}`}, children:[], alternate:[]});
  const internal = n => Array.from({length:n}, (_, i) => ({id:`effect-${i}`, kind:"effect", label:`write: v${i}`, ...witness, hidden:false, children:[], alternate:[]}));
  const view = steps => ({seed, participants, revision:{indexGeneration:"fixture", indexRevision:1}, semantic:{tier:"semantic"}, steps});
  // More than 200 internal steps, then one cross-lifeline call: drawn, nothing omitted.
  const late = view([...internal(240), call("late")]);
  const sparse = window.TrellisSequence.display(late, {interactions:true});
  assert.equal(sparse.interactions, true);
  assert.equal(sparse.returns.has(late.steps[240]), true, "the late interaction is drawn");
  assert.equal(sparse.omittedInteractions, 0, "no interaction was cut");
  assert.equal(sparse.hiddenSteps, 240);
  // 205 arrows: the first 200 are drawn in source order and 5 are counted as omitted.
  const dense = view([...internal(3), ...Array.from({length:205}, (_, n) => call(`call-${n}`))]);
  const capped = window.TrellisSequence.display(dense, {interactions:true});
  assert.equal(capped.returns.size, 200);
  assert.equal(capped.returns.has(dense.steps[202]), true);
  assert.equal(capped.returns.has(dense.steps[203]), false);
  assert.equal(capped.visible.has(dense.steps[203]), false);
  assert.equal(capped.omittedInteractions, 5);
  assert.equal(capped.hiddenSteps, 3, "omitted arrows are not internal steps");
});

// #716: a type `Session` and a module `session` never look alike, and a long lane name wraps
// between words instead of being clipped.
function namedColumns() {
  const {view, call, target} = resolvedSequence();
  const lane = (id, name, column) => {
    view.participants.push({id, label:name, kind:"internal", declaration:{...target, id, name}, column});
    return {...call, id:`call-${id}`, callId:`occurrence-${id}`, target:id};
  };
  view.steps = [
    lane("type", "open", {id:"type:Session", label:"Session"}),
    lane("module", "open", {id:"module:src/mcp/session.rs:session", label:"session"}),
    lane("class", "run", {id:"python:source:class:0123", label:"SemanticEvidenceProjection"}),
  ];
  return view;
}
test("#716: each Pele lane says whether it is a type, a module or a class, and wraps a long name", () => {
  const result = peleProjector().project(namedColumns());
  assert.deepEqual(Array.from(result.lanes, lane => Array.from(lane.lines)), [["run"], ["«type»", "Session"], ["«module»", "session"], ["«class»", "Semantic", "Evidence", "Projection"]],
    "a lane without a column has no kind; a name breaks only between words");
  assert.match(result.source, /^participant P1 as #171;type#187;<br\/>Session$/m);
  assert.match(result.source, /^participant P2 as #171;module#187;<br\/>session$/m);
  assert.match(result.source, /^participant P3 as #171;class#187;<br\/>Semantic<br\/>Evidence<br\/>Projection$/m);
});
test("#716: a lane name breaks after a separator or before a capital, never inside a word", () => {
  const window = {};
  vm.runInNewContext(sequenceSource, {window, document:{}});
  const lines = name => Array.from(window.TrellisSequence.laneHeader(undefined, name));
  assert.deepEqual(lines("behavior_columns"), ["behavior_", "columns"]);
  assert.deepEqual(lines("graph_read::tests"), ["graph_", "read::", "tests"]);
  assert.deepEqual(lines("fmt::Error"), ["fmt::Error"], "a short name stays on one line");
  assert.deepEqual(lines("sequence-pele.js"), ["sequence-", "pele.js"]);
  assert.deepEqual(lines("averyveryverylongunbrokenname"), ["averyveryverylongunbrokenname"], "a word is never cut");
});
