"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const window = {};
vm.runInNewContext(fs.readFileSync(path.join(__dirname, "../web/metric-heat.js"), "utf8"), {window});
const {createScale, quartiles, metricValue, relativeBand, exclusions} = window.TrellisMetricHeat;
const plain = value => JSON.parse(JSON.stringify(value));

const cutsByMetric = {
  cognitive:[5, 15, 30], cyclomatic:[5, 10, 20], maxNesting:[2, 3, 4],
  sloc:[30, 60, 120], parameters:[3, 5, 7], fanOut:[7, 15, 30], "halstead.volume":[500, 1000, 2000],
};
for (const [metric, cuts] of Object.entries(cutsByMetric)) {
  test(`${metric} keeps each owner boundary in its lower band for max and mean`, () => {
    for (const aggregate of ["max", "mean"]) {
      const scale = createScale(metric, aggregate, [0, ...cuts]);
      assert.match(scale.label, /Fixed per-callable bands/);
      assert.equal(scale.band(0), 0);
      cuts.forEach((cut, rank) => {
        assert.equal(scale.band(cut), rank, `${cut} boundary`);
        assert.equal(scale.band(cut + .01), rank + 1, `${cut} fractional successor`);
      });
      assert.deepEqual(plain(scale.entries), [
        {label:`≤${cuts[0]}`, band:0}, {label:`>${cuts[0]}–${cuts[1]}`, band:1},
        {label:`>${cuts[1]}–${cuts[2]}`, band:2}, {label:`>${cuts[2]}`, band:3},
      ]);
    }
  });
}

test("maintainability uses inclusive lower boundaries, including fractional means", () => {
  for (const aggregate of ["max", "mean"]) {
    const scale = createScale("maintainabilityIndex", aggregate, [0, 100]);
    for (const [value, expected] of [[100,0],[40,0],[39.99,1],[20,1],[19.99,2],[10,2],[9.99,3],[0,3]]) {
      assert.equal(scale.band(value), expected, String(value));
    }
    assert.match(scale.label, /higher is better; heat inverted/);
    assert.deepEqual(plain(scale.entries), [
      {label:"≥40",band:0}, {label:"≥20–<40",band:1}, {label:"≥10–<20",band:2}, {label:"<10",band:3},
    ]);
  }
});

test("fixed colours and legends do not depend on other visible values", () => {
  for (const metric of [...Object.keys(cutsByMetric), "maintainabilityIndex"]) {
    const original = createScale(metric, "max", [1, 12, 1000]);
    const filtered = createScale(metric, "max", [12]);
    assert.equal(original.band(12), filtered.band(12), metric);
    assert.equal(original.label, filtered.label, metric);
    assert.deepEqual(plain(original.entries), plain(filtered.entries), metric);
  }
});

test("sum defaults to neutral relative totals rather than callable risk bands", () => {
  for (const metric of ["cognitive", "maintainabilityIndex", "loc"]) {
    const scale = createScale(metric, "sum", [0, 1000]);
    assert.equal(scale.label, "Sum · relative totals; no callable risk bands");
    assert.equal(scale.band(1000), null);
    assert.deepEqual(plain(scale.entries), []);
  }
});

test("informational and unbanded metrics stay neutral by default", () => {
  for (const metric of ["loc", "commentLines", "commentRatio", "fanIn", "exits", "halstead.effort"]) {
    const scale = createScale(metric, "max", [0, 10, 100]);
    assert.equal(scale.band(100), null, metric);
    assert.match(scale.label, /Informational.*neutral/);
    assert.deepEqual(plain(scale.entries), []);
  }
});

test("Relative is opt-in, uses visible strict-cut quartiles and applies to sum and informational metrics", () => {
  const values = [0, 2, 5, 10];
  for (const [metric, aggregate] of [["cognitive","max"], ["cognitive","sum"], ["loc","mean"]]) {
    const scale = createScale(metric, aggregate, values, true);
    assert.match(scale.label, /Relative to visible set/);
    assert.deepEqual(plain(scale.entries), [
      {label:"≤2",band:0}, {label:">2–5",band:1}, {label:">5–10",band:2}, {label:">10",band:3},
    ]);
    assert.equal(scale.band(2), 0); assert.equal(scale.band(5), 1);
    assert.equal(scale.band(10), 2); assert.equal(scale.band(11), 3);
    assert.equal(createScale(metric, aggregate, [10], true).band(10), 0);
  }
  assert.equal(createScale("maintainabilityIndex", "sum", values, true).band(2), 3);
  assert.equal(createScale("maintainabilityIndex", "max", values, true).band(11), 0);
});

test("quartiles ignore unavailable values, keep zero and ties, and invert only MI", () => {
  assert.deepEqual(plain(quartiles([null, 0, 2, 5, 10, NaN, Infinity])), [2,5,10]);
  assert.deepEqual(plain(quartiles([])), []);
  assert.deepEqual(plain(quartiles([7])), [7,7,7]);
  assert.deepEqual(plain(quartiles([2,2,2,2])), [2,2,2]);
  assert.equal(relativeBand(2, [2,5,10], "cognitive"), 0);
  assert.equal(relativeBand(2, [2,5,10], "maintainabilityIndex"), 3);
  assert.equal(relativeBand(null, [2,5,10], "cognitive"), null);
  assert.equal(relativeBand(2, [], "cognitive"), null);
});

test("empty populations and missing numbers are neutral, never zero", () => {
  for (const [metric, aggregate, relative] of [["cognitive","max",false], ["maintainabilityIndex","mean",false], ["cognitive","sum",false], ["cognitive","max",true], ["loc","max",false]]) {
    const scale = createScale(metric, aggregate, [null, undefined, NaN, Infinity], relative);
    assert.equal(scale.band(10), null);
    assert.match(scale.label, /No measured values.*unavailable, not zero/);
    const expectedEntries = !relative && aggregate !== "sum" ? createScale(metric, aggregate, [0]).entries : [];
    assert.deepEqual(plain(scale.entries), plain(expectedEntries));
  }
  const scale = createScale("cognitive", "max", [0]);
  assert.equal(scale.band(0), 0);
  for (const value of [null, undefined, NaN, Infinity, "0"]) assert.equal(scale.band(value), null);
});

test("metricValue reads nested leaves without replacing unavailable data", () => {
  const metrics = {cognitive:0, halstead:{volume:500}, fanIn:{value:null, unavailable:"not-measured"}};
  assert.equal(metricValue(metrics, "cognitive"), 0);
  assert.equal(metricValue(metrics, "halstead.volume"), 500);
  assert.equal(metricValue(metrics, "fanIn.value"), null);
  assert.equal(metricValue(undefined, "halstead.volume"), undefined);
});


test("exclusions count raw minus selected callables once per row and count each excluded file once", () => {
  const rows = [
    {roleCounts:{production:2,test:3},declarationCount:2},
    {roleCounts:{production:4,test:1},declarationCount:0,excludedReason:"fixture"},
    {roleCounts:{production:2,test:0},declarationCount:0,excludedReason:"vendored"},
    {roleCounts:{production:1,test:2},declarationCount:0,excludedReason:"hidden_layer"},
    {roleCounts:{production:1,test:0},declarationCount:2},
    {declarationCount:0},
  ];
  assert.equal(exclusions(rows), "13 callables excluded from heat · excluded files: 1 fixture files, 1 vendored files, 1 authored hidden-layer files");
  assert.equal(exclusions([]), "0 callables excluded from heat · excluded files: 0 fixture files, 0 vendored files, 0 authored hidden-layer files");
  assert.match(exclusions([{roleCounts:{production:2,test:3},declarationCount:5}]), /^0 callables/);
});
