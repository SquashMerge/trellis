"use strict";
// Investigation harness: seeded interacting DTOs and a structural oracle, exercised in Chromium.
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const files = {"/vendor/pele.js":"web/vendor/pele-1.10.0-trellis.2.js", "/sequence.js":"web/sequence.js", "/sequence-pele.js":"web/sequence-pele.js", "/style.css":"web/style.css"};
const html = '<!doctype html><link rel="stylesheet" href="/style.css"><script src="/vendor/pele.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script><div id="diagram" style="width:1400px"></div>';
let server, browser, page;
test.before(async () => {
  server = createServer((req, res) => {
    if (files[req.url]) { res.setHeader("Content-Type", req.url.endsWith("css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8"); res.end(readFileSync(join(ROOT, files[req.url]))); }
    else { res.setHeader("Content-Type", "text/html; charset=utf-8"); res.end(html); }
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  browser = await chromium.launch(); page = await browser.newPage({viewport:{width:1600,height:900}});
  page.on("pageerror", error => console.error("browser error:", error.message));
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => window.TrellisPeleSequence && window.pele);
});
test.after(async () => { await browser?.close(); if (server) await new Promise(done => server.close(done)); });

function generator(seed) {
  let state = seed >>> 0;
  return n => { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return (state >>> 0) % n; };
}
function fixture(random) {
  const view = JSON.parse(readFileSync(join(ROOT, "tests/fixtures/sequence-pele/rust.json")));
  const template = view.steps[0], seed = view.seed;
  view.semantic = {tier:"semantic", producerId:"rust-analyzer"};
  const columns = {"fixture-seed":"A", a:"A", b:"B", b2:"B", c:"C"};
  view.participants = Object.entries(columns).map(([id, col]) => ({id, label:id, kind:"method", identification:"Exact target", column:{id:col,label:col}, declaration:{...structuredClone(seed), id, name:id, path:`${id}.rs`}}));
  let next = 0;
  const step = (kind, lane, extra = {}) => ({...structuredClone(template), id:`s${next++}`, kind, label:kind, lane:lane === seed.id ? undefined : lane, callId:null, children:[], alternate:[], hidden:false, ...extra});
  const effect = lane => step("effect", lane, {label:"bind name: value"});
  const exit = lane => step(["return", "throw", "exit"][random(3)], lane);
  const call = (lane, target, children = []) => {
    const result = step("call", lane, {label:"measured call", children});
    result.callId = `call:${result.id}`;
    if (target) { result.target = target; result.callEvidence = {boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${result.id}`}; }
    else result.callEvidence = {boundaryReason:"unresolved"};
    return result;
  };
  const loop = (lane, children) => step("loop", lane, {label:"for_expression: possible iterations, not unrolled; exit/termination unknown", children});
  const control = (lane, children, alternate = []) => {
    const kind = ["branch", "try", "boundary"][random(3)];
    return step(kind, lane, {label:kind === "boundary" ? "Nested callable boundary: body does not execute at definition; captures are not expanded." : kind, children, alternate});
  };
  // Every case has an expanded cross-column chain, a nested loop, an adjacent
  // loop, same-column and unresolved calls, and arrows before and after frames.
  const inside = call("b", "c");
  if (random(2)) inside.folded = [step("branch", "b", {label:"?: continue only on success; otherwise early return"})];
  const inner = loop("b", [effect("b"), inside, ...(random(2) ? [exit("b")] : [])]);
  const outer = loop("b", [call("b", "b2"), control("b", [call("b", null), inner]), call("b", "c")]);
  const adjacent = loop("b", [call("b", "c"), ...(random(2) ? [step("branch", "b", {label:"if done", children:[exit("b")]})] : []), call("b", "fixture-seed")]);
  const chain = call(seed.id, "b", [effect("b"), outer, adjacent, call("b", "b2", [call("b2", "c")])]);
  const randomSteps = [];
  for (let i = 0, n = random(5); i < n; i++) {
    const option = random(6);
    if (option === 0) randomSteps.push(call(seed.id, "c"));
    else if (option === 1) randomSteps.push(call(seed.id, "a", [call("a", "b")]));
    else if (option === 2) randomSteps.push(loop(seed.id, [effect(seed.id), call(seed.id, random(2) ? "b" : null)]));
    else if (option === 3) randomSteps.push(control(seed.id, [call(seed.id, "c")], [call(seed.id, null)]));
    else if (option === 4) randomSteps.push(call(seed.id, null));
    else randomSteps.push(step("group", seed.id, {label:"call chain", children:[call(seed.id, null), call(seed.id, "c")]}));
  }
  view.steps = [call(seed.id, "c"), ...randomSteps, chain, call(seed.id, "a"), call(seed.id, null), call(seed.id, "b")];
  view.hiddenSteps = 0; view.incidentalSteps = 0; view.warnings = [];
  return {view, columns};
}

// Interpret only the input DTO; no renderer helpers or projection state are used.
function model(view, columns) {
  const arrows = [], targets = [], endpoints = {}, calls = [], exact = [], loops = [], returns = [], stats = {same:0, unresolved:0, effects:0, exits:0, folded:0, nested:0, groups:0, closures:0, emptyLoops:0};
  function visit(steps, active) {
    for (const s of steps) {
      if (s.hidden) continue;
      const owner = columns[s.lane || view.seed.id];
      const isCall = s.kind === "call" && !!s.callId;
      const cross = isCall && s.target && columns[s.target] !== owner;
      if (isCall) { calls.push(s.id); if (!s.target) stats.unresolved++; else { exact.push(s.id); if (!cross) stats.same++; } }
      if (s.kind === "effect") stats.effects++;
      if (s.kind === "group") stats.groups++;
      if (s.kind === "boundary") stats.closures++;
      if (["return", "throw", "exit"].includes(s.kind)) { stats.exits++; for (const l of active) l.early = true; }
      if (s.folded?.length) { stats.folded++; for (const l of active) l.early = true; }
      let frame;
      if (s.kind === "loop") { frame = {members:[], early:false}; if (active.length) stats.nested++; loops.push(frame); active = [...active, frame]; }
      if (cross) { arrows.push(s.id); targets.push(s.target); endpoints[s.id] = [owner, columns[s.target]]; for (const l of active) l.members.push(s.id); }
      visit(s.children, active); visit(s.alternate, active);
      if (cross) returns.push(s.id);
      if (frame) { if (!frame.members.length) stats.emptyLoops++; active = active.slice(0, -1); }
    }
  }
  visit(view.steps, []);
  return {arrows, targets, endpoints, calls, exact, returns, loops:loops.filter(l => l.members.length), stats};
}

const sortedFrames = frames => frames.map(f => JSON.stringify(f)).sort();
test("generated Interactions arrows and exact loop spans agree with independent DTO model in both renderers", async () => {
  const seed = Number(process.env.TRELLIS_INTERACTIONS_SEED || 20261102), cases = Number(process.env.TRELLIS_INTERACTIONS_CASES || 32), random = generator(seed);
  const totals = {cases:0, arrows:0, frames:0, same:0, unresolved:0, effects:0, exits:0, folded:0, nested:0, groups:0, closures:0, emptyLoops:0, early:0, plain:0};
  for (let n = 0; n < cases; n++) {
    const {view, columns} = fixture(random), expected = model(view, columns);
    const actual = await page.evaluate(async view => {
      const container = document.querySelector("#diagram"), options = {interactions:true};
      const ids = (selector, attr) => [...container.querySelectorAll(selector)].map(e => e.getAttribute(attr));
      function measure(renderer) {
        const arrows = [...container.querySelectorAll(renderer === "pele" ? ".pele-edge-label[data-trellis-step-id]" : ".sequence-source[data-source-step-id]")];
        const attr = renderer === "pele" ? "data-trellis-step-id" : "data-source-step-id";
        const frames = [...container.querySelectorAll(renderer === "pele" ? ".pele-frame-loop" : '[data-presentation="interaction-loop"]')];
        const actorNodes = [...container.querySelectorAll(renderer === "pele" ? ".pele-actor:not(.pele-actor-footer)" : ".sequence-participant-label")];
        const actors = Object.fromEntries(actorNodes.map(node => {const r = node.getBoundingClientRect(); return [renderer === "pele" ? node.textContent : node.firstChild.textContent, (r.left+r.right)/2];}));
        const paths = [...container.querySelectorAll(renderer === "pele" ? ".pele-message[data-trellis-step-id] path" : ".sequence-arrow[data-trellis-call-id]")];
        const geometry = {};
        for (const path of paths) { const id = renderer === "pele" ? path.parentElement.getAttribute("data-trellis-step-id") : path.getAttribute("data-trellis-call-id").slice(5); const r = path.getBoundingClientRect(); geometry[id] ||= [r.left, r.right]; }
        return {arrows:arrows.map(e => e.getAttribute(attr)), targets:ids(renderer === "pele" ? ".pele-message[data-trellis-step-id]" : ".sequence-arrow[data-trellis-call-id]", "data-trellis-target-id"), returns:ids(renderer === "pele" ? ".pele-message[data-trellis-return-step-id]" : ".sequence-return[data-trellis-return-step-id]", "data-trellis-return-step-id"),
          actors, geometry,
          frames:frames.map(frame => { const rect = (renderer === "pele" ? frame.querySelector("rect, path") : frame.querySelector(":scope > .sequence-fragment")).getBoundingClientRect();
            return {members:arrows.filter(a => { const r = a.getBoundingClientRect(), y = (r.top+r.bottom)/2; return rect.top < y && y < rect.bottom; }).map(a => a.getAttribute(attr)), early:!!frame.querySelector(renderer === "pele" ? ":scope > .pele-frame-label" : ":scope > .sequence-control-label")}; })};
      }
      const full = {showDetails:true};
      const expanded = new Set(); const walk = items => items.forEach(s => {if (s.kind === "group") expanded.add(s.id); walk(s.children); walk(s.alternate);}); walk(view.steps);
      const fullPele = await TrellisPeleSequence.render(container, view, null, expanded, full);
      const fullPeleCalls = ids(".pele-message[data-trellis-step-id]", "data-trellis-step-id");
      TrellisSequence.render(container, view, null, expanded, full);
      const fullLegacyCalls = ids(".sequence-arrow[data-trellis-call-id]", "data-trellis-call-id");
      const pele = await TrellisPeleSequence.render(container, view, null, new Set(), options);
      const peleMeasured = measure("pele");
      TrellisSequence.render(container, view, null, new Set(), options);
      return {fullStatus:fullPele.status, fullReason:fullPele.reason, fullPeleCalls, fullLegacyCalls, status:pele.status, reason:pele.reason, pele:peleMeasured, legacy:measure("legacy")};
    }, view);
    const context = `seed=${seed} case=${n}`;
    assert.equal(actual.fullStatus, "rendered", `${context} full=${actual.fullReason}`);
    assert.equal(actual.status, "rendered", `${context} interaction=${actual.reason}`);
    assert.deepEqual(actual.fullPeleCalls, expected.exact, `${context} all full Pele exact calls`);
    assert.deepEqual(actual.fullLegacyCalls.map(id => id.slice(5)), expected.exact, `${context} all full legacy exact calls`);
    const eligible = new Set(expected.arrows);
    assert.deepEqual(actual.fullPeleCalls.filter(id => eligible.has(id)), expected.arrows, `${context} full Pele`);
    assert.deepEqual(actual.fullLegacyCalls.map(id => id.slice(5)).filter(id => eligible.has(id)), expected.arrows, `${context} full legacy`);
    for (const renderer of ["pele", "legacy"]) {
      assert.deepEqual(actual[renderer].arrows, expected.arrows, `${context} ${renderer} arrows`);
      assert.deepEqual(actual[renderer].targets, expected.targets, `${context} ${renderer} exact targets`);
      assert.deepEqual(actual[renderer].returns, expected.returns, `${context} ${renderer} returns`);
      assert.deepEqual(sortedFrames(actual[renderer].frames), sortedFrames(expected.loops), `${context} ${renderer} frames`);
      for (const id of expected.arrows) {
        const actualEnds = actual[renderer].geometry[id], expectedEnds = expected.endpoints[id].map(col => actual[renderer].actors[col]).sort((a, b) => a - b);
        assert.ok(actualEnds && expectedEnds.every((x, i) => Number.isFinite(x) && Math.abs(x - actualEnds[i]) < 20), `${context} ${renderer} ${id} endpoints ${JSON.stringify({actualEnds, expectedEnds})}`);
      }
    }
    totals.cases++; totals.arrows += expected.arrows.length; totals.frames += expected.loops.length; totals.early += expected.loops.filter(l => l.early).length;
    totals.plain += expected.loops.filter(l => !l.early).length;
    for (const key of Object.keys(expected.stats)) totals[key] += expected.stats[key];
  }
  for (const [key, value] of Object.entries(totals)) assert.ok(value > 0, `vacuous ${key}`);
  console.log(`seed=${seed} coverage=${JSON.stringify(totals)}`);
});
