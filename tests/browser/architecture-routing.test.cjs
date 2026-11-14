"use strict";
// #637: arrow routing on Trellis's own Architecture summary, in real Chromium under the product
// CSP. The fixture is this repository's captured summary (native index plus pinned
// rust-analyzer and scip-typescript imports), trimmed to what the drawing reads: it has long
// upward deviations from storage, long downward edges, adjacent and shared-box arrows.
const test = require("node:test"), assert = require("node:assert/strict"), fs = require("node:fs"), path = require("node:path"), http = require("node:http");
const {chromium} = require("playwright");
const {arrowGeometry} = require("./architecture-geometry.cjs");
const root = path.resolve(__dirname, "../..");
const summary = JSON.parse(fs.readFileSync(path.join(root, "tests/fixtures/architecture/trellis-summary.json"), "utf8"));

async function withPage(callback) {
  const routes = {"/vendor.js":"web/vendor/pele-1.10.0-trellis.2.js", "/pele-svg.js":"web/pele-svg.js", "/adapter.js":"web/architecture-pele.js"};
  const server = http.createServer((req, res) => {
    res.setHeader("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'");
    if (routes[req.url]) { res.setHeader("Content-Type", "text/javascript; charset=utf-8"); res.end(fs.readFileSync(path.join(root, routes[req.url]))); }
    else { res.setHeader("Content-Type", "text/html; charset=utf-8"); res.end('<!doctype html><div id="canvas"></div><script src="/vendor.js"></script><script src="/pele-svg.js"></script><script src="/adapter.js"></script>'); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  let browser;
  try {
    browser = await chromium.launch();
    const page = await browser.newPage({viewport:{width:1440, height:1000}}), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await callback(page);
    assert.deepEqual(errors, []);
  } finally { if (browser) await browser.close(); await new Promise(resolve => server.close(resolve)); }
}

// The summary display as the app draws it: hidden and test-fixture layers left out, every band
// collapsed to a row of its hottest module chips and a "+N more" chip, header count chips and
// bounded counts with their mapped share on deviating arrows.
async function draw(page, view, {width, showTests = false}) {
  return page.evaluate(async ({view, width, showTests}) => {
    const A = window.TrellisArchitecturePele, canvas = document.querySelector("#canvas");
    const hidden = new Set(view.layers.filter(layer => layer.display === "hidden" || layer.name === "fixture-inputs").map(layer => layer.name));
    const byName = new Map(view.layers.map(layer => [layer.name, layer]));
    const stats = layer => {
      const counts = {up:0, down:0, shared:0};
      for (const edge of view.edges) if (edge.sourceLayer === layer.name && edge.role === "production") {
        const to = byName.get(edge.destinationLayer);
        if (to.shared) counts.shared += edge.count; else if (!layer.shared && to.order < layer.order) counts.up += edge.count; else counts.down += edge.count;
      }
      return [["up", "↑", "error"], ["down", "↓", "neutral"], ["shared", "→shared ", "neutral"]].filter(([key]) => counts[key] > 0)
        .map(([key, glyph, tone]) => ({text:`${glyph}≥${counts[key]}`, label:`${counts[key]} ${key}`, tone}));
    };
    const options = {
      showTests, width, metric:"cognitive",
      moduleValue:module => module.cognitive, moduleBand:module => module.cognitive === null ? null : Math.min(3, Math.floor(module.cognitive / 40)),
      layerLabel:layer => `${layer.shared ? "Shared" : layer.order} · ${layer.name} · ${view.modules.filter(module => module.layer === layer.name).length} files`,
      layerStats:stats,
      edgeText:edge => ({bound:!edge.coverage?.complete, label:"lower bound", note:edge.status === "deviates" && edge.coverage ? `${Math.round(edge.coverage.mappedSites / edge.coverage.sites * 100)}% mapped` : ""}),
      edgeTone:edge => edge.role === "test" ? "neutral" : edge.status === "deviates" && !edge.excepted ? "deviation" : "allowed",
      display:{layers:new Set(view.layers.filter(layer => !hidden.has(layer.name)).map(layer => layer.name)), expanded:() => false, edge:() => true, entries:() => [],
        summary:(key, modules, columns) => {
          const hot = [...modules].sort((a, b) => (b.cognitive ?? -1) - (a.cognitive ?? -1)), shown = hot.slice(0, Math.max(1, columns - 1));
          const chips = shown.map(module => ({type:"chip", label:module.identity.path, shown:`${module.identity.path.split("/").pop()}\n${module.cognitive ?? "—"}`, band:module.cognitive === null ? null : Math.min(3, Math.floor(module.cognitive / 40))}));
          if (hot.length > shown.length) chips.push({type:"chip", label:`${hot.length - shown.length} more`, shown:`+${hot.length - shown.length} more\nopen band`, band:null});
          return chips;
        },
        unassigned:{label:`Unassigned / unknown\n${view.modules.filter(module => module.layer === null).length} files · no layer`, expanded:false}},
      onToggleLayer() {}, onSelectEdge() {}, onSelectModule() {}, onToggleUnassigned() {}, onSelectNote() {}, onToggleDirectory() {},
    };
    const outcome = await A.render(canvas, view, options);
    return {status:outcome.status, error:outcome.error?.message, width:canvas.querySelector("svg")?.getBoundingClientRect().width};
  }, {view, width, showTests});
}

function assertRouted(geometry, label, longUpward = 2) {
  assert.equal(geometry.error, undefined, geometry.error);
  assert.ok(geometry.bands >= 6 && geometry.arrows > 0, `${label}: drew ${geometry.bands} bands and ${geometry.arrows} arrows`);
  assert.deepEqual(geometry.crossings, [], `${label}: no arrow segment enters a band or a module box`);
  assert.deepEqual(geometry.tagHits, [], `${label}: count tags overlap no band, chip or other tag`);
  assert.deepEqual(geometry.detached, [], `${label}: every arrow starts on its source band and ends on its target band`);
  const long = geometry.upward.filter(arrow => arrow.long);
  assert.ok(long.length >= longUpward, `${label}: the fixture has long upward arrows: ${JSON.stringify(geometry.upward)}`);
  assert.deepEqual(long.filter(arrow => !arrow.channel).map(arrow => arrow.name), [], `${label}: every long upward arrow runs in the side channel`);
}

test("#637 routes Trellis's own Architecture arrows through gutters and the side channel at 1440, 1100 and 900", {timeout:120000}, async () => withPage(async page => {
  for (const width of [1400, 1060, 860]) for (const showTests of [false, true]) {
    const drawn = await draw(page, summary, {width, showTests});
    assert.equal(drawn.status, "ready", drawn.error);
    assert.ok(drawn.width <= width + 0.5, `${width}: the routed drawing, channels included, fits the width it was given (${drawn.width})`);
    assertRouted(await page.evaluate(arrowGeometry, "#canvas svg"), `${width}px${showTests ? " with tests" : ""}`);
  }
}));

test("#637 arrow width follows the reference count on a log scale, clamped at the minimum and maximum", {timeout:120000}, async () => withPage(async page => {
  const scale = await page.evaluate(() => [0, 1, 2, 9, 10, 100, 999, 1000, 5000, 1e6].map(count => [count, window.TrellisArchitecturePele.edgeWidth(count)]));
  const widths = scale.map(([, width]) => width);
  assert.deepEqual([widths[0], widths[1]], [1.5, 1.5], "one site or fewer draws the minimum width");
  assert.deepEqual(widths.slice(-3), [7, 7, 7], "1000 sites or more draws the maximum width");
  for (let i = 1; i < widths.length; i++) assert.ok(widths[i] >= widths[i - 1], `widths never shrink as counts grow: ${JSON.stringify(scale)}`);
  assert.ok(widths[4] > widths[2] && widths[5] > widths[4] && widths[7] > widths[5], "each tenfold count draws a wider arrow until the maximum");
  // Drawn arrows: ordered by their counts, between the clamps, with the exact count still in the tag.
  await draw(page, summary, {width:1400});
  const geometry = await page.evaluate(arrowGeometry, "#canvas svg");
  const drawn = [...geometry.widths].sort((a, b) => a.count - b.count);
  for (let i = 1; i < drawn.length; i++) assert.ok(drawn[i].width >= drawn[i - 1].width, `drawn widths follow counts: ${JSON.stringify(drawn)}`);
  assert.ok(drawn.every(arrow => arrow.width >= 1.5 && arrow.width <= 7), JSON.stringify(drawn));
  assert.ok(drawn[drawn.length - 1].width > drawn[0].width, "the largest edge is drawn wider than the smallest");
  assert.ok(drawn.every(arrow => arrow.name.includes(String(arrow.count))), "each tag keeps its exact count");
}));

test("#637 routes peers, adjacent rows, two shared boxes and shared-to-shared arrows without crossings", {timeout:120000}, async () => withPage(async page => {
  // A synthetic summary that exercises every route: peers in one row, a long downward and a
  // long upward arrow, a nearer and a farther shared box, and an arrow between shared boxes.
  const layer = (name, order, shared = false) => ({name, order:shared ? null : order, shared, display:null});
  const module = (name, layerName, i) => ({identity:{sourceSetId:"s", language:"rust", path:`src/${name}_${i}.rs`, sid:`sid:${name}:${i}`}, layer:layerName, cognitive:i * 17});
  const names = [["top", 1], ["left", 2], ["right", 2], ["middle", 3], ["low", 4], ["alpha", null, true], ["beta", null, true]];
  const view = {state:"ready", layers:names.map(([name, order, shared]) => layer(name, order, shared)),
    modules:names.flatMap(([name]) => [0, 1, 2, 3, 4, 5].map(i => module(name, name, i))),
    edges:[["top", "left", 40], ["left", "right", 3], ["right", "left", 2], ["top", "low", 900], ["low", "top", 12], ["low", "middle", 5], ["middle", "low", 2000],
      ["left", "alpha", 30], ["low", "beta", 7], ["beta", "middle", 1], ["alpha", "beta", 4], ["middle", "right", 9]]
      .map(([sourceLayer, destinationLayer, count]) => {
        const s = names.find(n => n[0] === sourceLayer), d = names.find(n => n[0] === destinationLayer);
        const deviates = s[2] ? true : d[2] ? false : d[1] < s[1] || d[1] === s[1];
        return {sourceLayer, destinationLayer, role:"production", status:deviates ? "deviates" : "conforms", count, excepted:false, exceptionReason:null, coverage:{complete:false, mappedSites:count, sites:count * 2}};
      })};
  for (const width of [1400, 900]) {
    const drawn = await draw(page, view, {width});
    assert.equal(drawn.status, "ready", drawn.error);
    const geometry = await page.evaluate(arrowGeometry, "#canvas svg");
    assert.equal(geometry.arrows, view.edges.length);
    assertRouted(geometry, `synthetic ${width}px`, 1);
  }
}));

test("#637 the summary display admits Pele's boxes only and refuses any connector Pele draws", {timeout:120000}, async () => withPage(async page => {
  const result = await page.evaluate(view => {
    const A = window.TrellisArchitecturePele;
    const p = A.project(view, {display:{layers:new Set(view.layers.filter(layer => layer.display !== "hidden").map(layer => layer.name)), expanded:() => false, edge:() => true, entries:() => [], summary:() => [], unassigned:null}});
    const raw = window.pele.render(p.source, {fontSize:14, padding:8, responsive:false, links:false, images:false}).svg;
    const [first] = p.edges.values();
    const connector = `<g class="pele-edges" fill="none" stroke="var(--_l)"><g class="pele-edge" data-id="1-${first.from}-${first.to}"><path d="M0,0L9,9"/><path class="pele-marker" d="M9,9L6,6L6,9Z" fill="var(--_l)" stroke="none"/></g></g>`;
    const admitted = A.validatedSvg(raw, p).controls.size;
    let refused = null;
    try { A.validatedSvg(raw.replace("</svg>", `${connector}</svg>`), p); } catch (error) { refused = error.message; }
    return {edges:p.edges.size, admitted, nodes:p.nodes.size, refused, source:p.source.includes("-->")};
  }, summary);
  assert.ok(result.edges > 0);
  assert.equal(result.source, false, "the summary source gives Pele no edges");
  assert.equal(result.admitted, result.nodes + result.edges, "Pele's boxes and the adapter's own arrows are all controls");
  assert.equal(result.refused, "Missing or unexpected Architecture edge");
}));
