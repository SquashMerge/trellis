"use strict";
// web/architecture-routing.js (#1054): the arrow routing arithmetic, with no DOM. The golden
// table (tests/fixtures/architecture/routing-golden.json) is fixed drawings as frames, with the
// polylines, tag centres, moved frames and drawing size the routing gave before it left the Pele
// adapter: every number must stay exactly what it was.
const test = require("node:test"), assert = require("node:assert/strict"), fs = require("node:fs"), vm = require("node:vm");
const source = fs.readFileSync("web/architecture-routing.js", "utf8");
const golden = JSON.parse(fs.readFileSync("tests/fixtures/architecture/routing-golden.json", "utf8"));
// The script gets a bare `window` and nothing else: no document, no DOM classes.
function load() { const window = {}; vm.runInNewContext(source, {window}); return window.TrellisArchitectureRouting; }
const R = load();
const plain = value => JSON.parse(JSON.stringify(value));

// What the adapter does with `placement`: move each box (unrounded when it does not move) and
// grow a shared box, the unassigned row and the stack.
function moved(routing, id, frame, host) {
  const at = R.placement(routing, host), {stack, total, inner} = routing;
  const out = at.dx || at.dy ? {...frame, x:R.round(frame.x + at.dx), y:R.round(frame.y + at.dy)} : {...frame};
  if (at.part === "shared") out.h = R.round(Math.max(out.h + total, stack.y + stack.h + total - out.y));
  if (at.part === "unassigned") out.w = R.round(out.w + inner);
  if (at.part === "stack") out.h = R.round(out.h + total);
  return out;
}
function route(entry) {
  const {layout, stack, frames, edges, root} = entry;
  const routing = R.plan({layout, stack, edges, band:id => frames[id], tag:edge => edge.tag});
  const after = {[layout.stack]:moved(routing, layout.stack, stack, null)};
  for (const id of Object.keys(frames)) after[id] = moved(routing, id, frames[id], id);
  const arrows = [];
  R.paths(routing, id => after[id], (edge, points, tag) => arrows.push({id:edge.id, points, tag}));
  return {routing, moved:after, arrows, size:{width:R.round(root.width + routing.grow), height:R.round(root.height + routing.total + routing.stripH)}};
}

test("the script is DOM-free and publishes one frozen global", () => {
  assert.ok(Object.isFrozen(R));
  assert.deepEqual(Object.keys(R).sort(), ["PATH", "PLAN", "channelWidth", "countLanes", "paths", "placement", "plan", "relation", "round", "slots"]);
  assert.doesNotMatch(source.replace(/^\s*\/\/.*$/gm, ""), /\bdocument\b|\bwindow\.(?!TrellisArchitectureRouting\b)|createElement|querySelector|getAttribute|setAttribute|addEventListener|classList|getComputedStyle/);
});

test("golden drawings: polylines, tag centres, moved frames and size are exactly as before", () => {
  const kinds = new Set();
  for (const entry of golden) {
    const result = route(entry);
    for (const plan of result.routing.plans) kinds.add(plan.kind);
    assert.deepEqual(plain(result.arrows), entry.expect.arrows, entry.name);
    assert.deepEqual(plain(result.moved), entry.expect.moved, entry.name);
    assert.deepEqual(plain(result.size), entry.expect.size, entry.name);
  }
  // The table reaches every plan kind, and an arrow with no plan.
  assert.deepEqual([...kinds].map(String).sort(), ["boxes", "far", "jog", "left", "peer", "right", "shared", "straight", "undefined"]);
});

test("the plan table has one row per relation, and the path table one per plan kind", () => {
  assert.deepEqual(Object.keys(R.PLAN).sort(), ["adjacent", "boxes", "down", "peer", "shared", "up"]);
  assert.deepEqual(Object.keys(R.PATH).sort(), ["boxes", "far", "jog", "left", "peer", "right", "shared", "straight"]);
  // rows of the two ends, then their shared-box indexes
  const cases = [[[0, 1], "adjacent"], [[1, 0], "adjacent"], [[2, 2], "peer"], [[0, 2], "down"], [[3, 0], "up"],
    [[1, undefined, undefined, 0], "shared"], [[undefined, 0, 2, undefined], "shared"], [[1, undefined], "shared"],
    [[undefined, undefined, 0, 1], "boxes"], [[undefined, undefined, 0, 0], "boxes"], [[undefined, undefined, 0, undefined], null], [[undefined, undefined], null]];
  for (const [args, expected] of cases) {
    assert.equal(R.relation(...args), expected, JSON.stringify(args));
    if (expected) assert.equal(typeof R.PLAN[expected], "function");
  }
  // Each relation's plan kinds, from the golden drawings, all have a path row.
  const produced = new Map();
  for (const entry of golden) for (const plan of route(entry).routing.plans) {
    const rowOf = new Map(entry.layout.rows.flatMap((ids, row) => ids.map(id => [id, row]))), sharedOf = new Map(entry.layout.shared.map((id, index) => [id, index]));
    const key = R.relation(rowOf.get(plan.edge.from), rowOf.get(plan.edge.to), sharedOf.get(plan.edge.from), sharedOf.get(plan.edge.to));
    if (!produced.has(key)) produced.set(key, new Set());
    produced.get(key).add(plan.kind);
    if (plan.kind) assert.equal(typeof R.PATH[plan.kind], "function", plan.kind);
  }
  assert.deepEqual(Object.fromEntries([...produced].map(([key, kinds]) => [key, [...kinds].map(String).sort()])),
    {adjacent:["jog", "straight"], peer:["peer"], down:["right"], up:["left"], shared:["far", "shared"], boxes:["boxes"], null:["undefined"]});
});

test("side-channel lanes are counted by the same relations, and sized by lanes", () => {
  const layout = {rows:[["A", "B"], ["C"], ["D"]], shared:["S0", "S1"]};
  const lanes = edges => plain(R.countLanes(layout, edges.map(([from, to]) => ({from, to}))));
  assert.deepEqual(lanes([]), {left:0, right:0});
  assert.deepEqual(lanes([["A", "C"], ["C", "A"], ["A", "B"], ["A", "A"]]), {left:0, right:0}, "adjacent rows and peers take no lane");
  assert.deepEqual(lanes([["A", "D"], ["B", "D"], ["D", "A"]]), {left:1, right:2}, "long down goes right, long up goes left");
  assert.deepEqual(lanes([["A", "S0"], ["S0", "C"]]), {left:0, right:0}, "the nearest shared box needs no lane");
  assert.deepEqual(lanes([["A", "S1"], ["S1", "C"], ["S0", "S1"], ["A", "nowhere"]]), {left:0, right:2}, "a farther shared box takes a right lane each way");
  assert.deepEqual([0, 1, 3].map(lanes => R.channelWidth(lanes)), [0, 26, 46]);
  assert.deepEqual([1.005, 2.675, 12.3449, 7].map(value => R.round(value)), [1, 2.68, 12.34, 7]);
});

test("slots: runs keep clear of each other, and tags that overlap take another row", () => {
  // Untagged runs: the first takes its preferred x, the second scans outward to stay a lane and a gap away.
  const runs = [{lo:0, hi:100, pref:50}, {lo:0, hi:100, pref:50}];
  assert.equal(R.slots(runs), 0);
  assert.deepEqual(runs.map(run => run.x), [50, 34]);
  // A tag whose box cannot clear the first run's line keeps its preferred x in a new tag row; one
  // too wide for its span centres on it.
  const tags = [{lo:0, hi:100, pref:50, tag:{w:60, h:18}}, {lo:0, hi:100, pref:50, tag:{w:60, h:18}}, {lo:200, hi:220, pref:210, tag:{w:60, h:18}}];
  assert.equal(R.slots(tags), 2);
  assert.deepEqual(tags.map(item => [item.x, item.row]), [[50, 0], [50, 1], [210, 0]]);
});

test("placement: which part moves with what", () => {
  const entry = golden.find(item => item.name.startsWith("shared boxes right")), {routing} = route(entry);
  const placement = owner => plain(R.placement(routing, owner));
  assert.deepEqual(placement("A"), {part:"row", dx:routing.stackDx, dy:0});
  assert.deepEqual(placement("F"), {part:"row", dx:routing.stackDx, dy:routing.below(3)});
  assert.deepEqual(placement("S1"), {part:"shared", dx:routing.sharedDx, dy:0});
  assert.deepEqual(placement("U"), {part:"unassigned", dx:routing.baseDx, dy:routing.total + routing.stripH});
  assert.deepEqual(placement(null), {part:"stack", dx:routing.stackDx, dy:0});
  assert.ok(routing.total > 0 && routing.stripH > 0 && routing.grow > 0);
});
