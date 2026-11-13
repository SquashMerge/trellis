// Parity with the web app's Architecture view (#886): colours and Color by, the app switcher
// and per-app columns, the HTTP interface card and stubs, deviations and the Deviations tree,
// the status line and the logo. The fixtures are a real daemon's answers for a two-app
// checkout (a Rust server `srv` with an upward deviation, and a JavaScript client `webapp`).
//   node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/parity.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEFAULT_METRIC, apps, deviationModel, draw, summarize } from "../src/diagram.js";
import { state } from "./stubs/herdr.js";
import Trellis from "../src/main.js";

const read = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8"));
const twoApps = () => read("apps-architecture.json");
// The negative control: the same checkout with only its server app, as a one-app checkout answers.
function oneApp() {
  const v = twoApps();
  return { ...v, apps: v.apps.filter((a) => a.name === "srv"), interfaces: [], layers: v.layers.filter((l) => l.app === "srv"),
    modules: v.modules.filter((m) => m.app === "srv") };
}
const theme = { background: "#101114", foreground: "#e6e6e6", muted: "#a0a0a8", input: "#6a6a78", dim: "#777777", surface: "#17181c", raised: "#202128",
  border: "#2c2d35", hover: "#25262c", selected: "#2a2f3a", accent: "#7aa2f7", ansi2: "#98c379", ansi3: "#e5c07b" };
const css = readFileSync(new URL("../../web/style.css", import.meta.url), "utf8");
const token = (name) => new RegExp(`--${name}:\\s*(#[0-9A-Fa-f]{6})`).exec(css)[1].toUpperCase();
const html = readFileSync(new URL("../../web/index.html", import.meta.url), "utf8");
const fills = (svg) => new Set([...svg.matchAll(/fill="(#[0-9A-Fa-f]{6})"/g)].map((m) => m[1].toUpperCase()));

test("colours: the web app's Color by default, its heat scale and its heat tokens", async () => {
  assert.equal(DEFAULT_METRIC, /<option value="([^"]+)" selected>/.exec(/<select id="architecture-color">[\s\S]*?<\/select>/.exec(html)[0])[1]);
  const view = twoApps(), summary = summarize(view, { scope: "srv" });
  assert.equal(summary.legendText(summary.scale), "Fixed per-callable bands · cognitive · max · production callables");
  // The hottest module (cognitive 10, in (5, 15]) is heat band 1, drawn in the web's --sm-gold.
  assert.equal(summary.scale.band(10), 1);
  const [image] = (await draw(view, { width: 600, theme, highlight: null, choice: { scope: "srv" } })).images;
  assert.ok(fills(image.svg).has(token("sm-gold")), "heat band 1 is --sm-gold, not a theme colour");
  // Another metric re-scales the same payload: max nesting 2 is band 0, so no heat fill remains.
  const nesting = summarize(view, { scope: "srv", metric: "maxNesting" });
  assert.equal(nesting.legendText(nesting.scale), "Fixed per-callable bands · maxNesting · max · production callables");
  const [cool] = (await draw(view, { width: 600, theme, highlight: null, choice: { scope: "srv", metric: "layer" } })).images;
  for (const heat of [token("sm-gold"), token("sm-amber"), token("sm-orange")]) assert.ok(!fills(cool.svg).has(heat), `Layer colouring has no heat (${heat})`);
});

test("apps: the switcher's choices, one column per app, side by side or stacked, and one app's stack", async () => {
  const view = twoApps(), summary = summarize(view);
  assert.deepEqual(summary.choices.map((c) => c.text), ["All apps", "srv · rust", "webapp · javascript"]);
  assert.equal(apps.sourceLine(summary.group, view.apps.length), "2 apps · hidden layers and unassigned files grouped below");
  const stacked = await draw(view, { width: 600, theme, highlight: null, choice: {} });
  assert.equal(stacked.sideBySide, false);
  assert.equal(stacked.images.length, 2, "one drawing per app column");
  assert.match(stacked.images[0].svg, /srv · ordered/);
  assert.match(stacked.images[1].svg, /webapp · ordered/, "the title wraps at this width");
  const wide = await draw(view, { width: 1300, theme, highlight: null, choice: {} });
  assert.equal(wide.sideBySide, true);
  assert.equal(wide.images.length, 1, "side by side is one composed drawing");
  assert.match(wide.images[0].svg, /srv · ordered[\s\S]*webapp · ordered/);
  assert.match(wide.images[0].svg, /<path d="M[^"]+" fill="none" stroke="#6a6a78"/, "the interface runs as a line between the columns");
  // One app: its own stack, titled, with no other app's layers.
  const one = await draw(view, { width: 600, theme, highlight: null, choice: { scope: "webapp" } });
  assert.equal(one.images.length, 1);
  assert.match(one.images[0].svg, /webapp · ordered/);
  assert.doesNotMatch(one.images[0].svg, /server|core/);
});

test("HTTP interfaces: the card's DTO counts, its inspector facts and the band stubs", async () => {
  const view = twoApps();
  const out = await draw(view, { width: 600, theme, highlight: null, choice: {} });
  assert.deepEqual(out.cards.map((c) => c.lines.map((l) => l.text)), [["HTTP · 1 route", "1 call site", "1 exact · 1 unresolved", "◆ 1 server-only", "◆ 1 client-only"]]);
  const facts = apps.interfaceFacts(view.interfaces[0]);
  assert.equal(facts.title, "webapp → srv");
  assert.deepEqual(facts.ends, ["Client end: webapp · client", "Server end: srv · server"]);
  assert.deepEqual(facts.rows.slice(0, 2), [["Matched routes", "1"], ["Client call sites", "1"]]);
  const stubs = out.images.flatMap((i) => i.boxes).filter((b) => b.kind === "A" && b.action[0] === "interface").map((b) => b.label);
  assert.deepEqual(stubs.sort(), ["HTTP interface from webapp: 1 route", "HTTP interface to srv: 1 route, 1 call site"]);
  assert.ok(out.images.every((i) => !i.svg.includes(">Selected<")), "the stub's Selected word is the browser's pressed state only");
});

test("deviations: the counts, the column header, the ↑ chip and the tree model over a real detail page", async () => {
  const view = twoApps(), summary = summarize(view);
  assert.equal(summary.reviewCounts.deviates, 2);
  assert.equal(summary.reviewCounts.unassigned, 0);
  const srv = summary.plan.parts.find((p) => p.group.name === "srv");
  assert.equal(summary.columnHeader(srv).deviations, "2 deviations");
  const out = await draw(view, { width: 600, theme, highlight: null, choice: {} });
  const up = out.images.flatMap((i) => i.boxes).find((b) => b.kind === "A" && b.action[0] === "up");
  assert.deepEqual(up.action, ["up", "core", "server"]);
  const rows = read("apps-deviations.json").payload.items;
  assert.ok(rows.every(deviationModel.valid));
  const tree = deviationModel.build(rows, view.layers, null);
  assert.deepEqual(tree.files.map((f) => f.path), ["src/util.rs"]);
  assert.deepEqual(deviationModel.chips(view, summary.edgeText).edges.map((e) => [e.key, e.count]), [["core→server", 3]]);
});

test("negative control: a one-app checkout has no switcher, no columns, no cards and no stubs", async () => {
  const view = oneApp(), summary = summarize(view, { scope: "srv" });
  assert.deepEqual(summary.choices, []);
  assert.equal(summary.scope, null, "with one app there is no scope to choose");
  const out = await draw(view, { width: 1300, theme, highlight: null, choice: { scope: "srv" } });
  assert.equal(out.images.length, 1);
  assert.equal(out.cards.length, 0);
  assert.doesNotMatch(out.images[0].svg, /srv · ordered layers/, "the checkout's one stack, untitled by app");
  assert.ok(!out.images[0].boxes.some((b) => b.kind === "A" && b.action[0] === "interface"));
  assert.equal(summary.reviewCounts.deviates, 2, "the deviations still count");
});

// --- the pane, end to end against a fake daemon ---

function fakeDaemon(payload, { job = null, details = null } = {}) {
  const at = { indexGeneration: payload.indexGeneration, indexRevision: payload.indexRevision }, seen = [], TOKEN = "b".repeat(64), KEY = "a".repeat(64);
  const server = createServer((req, res) => {
    seen.push(req.url);
    const send = (status, value) => { res.writeHead(status, { "Content-Type": "application/json" }); res.end(JSON.stringify(value)); };
    if (req.headers.authorization !== `Bearer ${TOKEN}`) return send(401, { error: { code: "unauthorized", message: "no" } });
    const url = new URL(req.url, "http://x"), base = `/api/checkouts/${KEY}/`;
    if (url.pathname === "/api/checkouts") return send(200, { checkouts: [{ rootKey: KEY, state: "available", workspaceRoot: "/work/apps", revision: at, catchingUp: false }] });
    if (url.pathname === `${base}status`) return send(200, { workspaceRoot: "/work/apps", revision: at, catchingUp: false });
    if (url.pathname === `${base}jobs/current`) return send(200, job);
    if (url.pathname === `${base}architecture`) return send(200, { payload: { ...payload, aggregate: url.searchParams.get("aggregate") } });
    if (url.pathname === `${base}architecture/details`) {
      assert.equal(url.searchParams.get("rulesHash"), payload.rulesFile.contentHash);
      return send(200, details ?? read("apps-deviations.json"));
    }
    send(404, { error: { code: "not_found", message: "No such route" } });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, seen, port: server.address().port, TOKEN })));
}

async function pane(payload, options) {
  const daemon = await fakeDaemon(payload, options);
  const dir = mkdtempSync(join(tmpdir(), "trellis-parity-")), tokenFile = join(dir, "token");
  (await import("node:fs")).writeFileSync(tokenFile, `${daemon.TOKEN}\n`);
  Object.assign(state, { appHome: dir, focused: { cwd: "/work/apps" }, shared: {}, spaces: [] });
  const view = new Trellis();
  let tasks = 0, last;
  const cx = {
    spawn(fn) { tasks++; Promise.resolve().then(() => fn(cx)).finally(() => tasks--); return { cancel() {} }; },
    // The draw watchdog (15 s) is cancelled when a drawing finishes, so it never counts as work.
    timer: { every() { return { cancel() {} }; }, after(ms, fn) {
      const long = ms >= 10000; if (!long) tasks++;
      let done = false; const end = () => { if (!done) { done = true; if (!long) tasks--; } };
      const id = setTimeout(() => { end(); fn(cx); }, ms); return { cancel() { clearTimeout(id); end(); } };
    } },
    sleep: (ms = 0) => new Promise((r) => setTimeout(r, ms)), notify() {},
    // GPUI's: a handler stops the press reaching the handlers of the elements around it.
    stopped: 0, stop_propagation() { cx.stopped++; },
  };
  const settle = async () => {
    for (let quiet = 0, rounds = 0; quiet < 3; rounds++) {
      if (rounds > 300) throw new Error("the pane never settled");
      await new Promise((r) => setTimeout(r, 20)); last = view.render(cx); quiet = tasks ? 0 : quiet + 1;
    }
    return last;
  };
  state.props = { url: `http://127.0.0.1:${daemon.port}`, tokenFile };
  view.init(state.props, cx);
  await settle();
  last.find("root").props.size({ width: 900, height: 800 }, cx);
  const click = async (id) => { const el = last.find(id); assert.ok(el, `no element ${id} in: ${last.text().slice(0, 300)}`); el.props.click({}, cx); return settle(); };
  const close = async () => { await view.close(); daemon.server.close(); rmSync(dir, { recursive: true, force: true }); };
  return { view, cx, settle, click, close, seen: daemon.seen, root: () => last };
}

test("#911 a deviation line reads its target's name, never the internal declaration key", async () => {
  const details = read("apps-deviations.json"), site = details.payload.items[0].instance.evidence[0];
  site.target = { syntaxId: "sid:v1:911", displayKey: "src/api.rs#implementation@4.enqueue_generated_semantic_import",
    document: { sourceSetId: site.document.sourceSetId, language: "rust", path: "src/api.rs" }, lines: { start: 3, end: 5 } };
  site.targetReason = null;
  const p = await pane(twoApps(), { details });
  try {
    await p.settle();
    await p.click("review-deviations");
    await p.click("deviation-file-0");
    const root = await p.click("deviation-fn-1");
    const tree = root.find("deviation-tree").text();
    assert.match(tree, /L2 enqueue_generated_semantic_import call → api\.rs:3/, "the line shows the method's name and its definition");
    assert.doesNotMatch(tree, /implementation@4|src\/api\.rs#/, "the internal key is never the label");
  } finally { await p.close(); }
});

test("the pane: logo, status line, switcher, columns, interface card, deviations entry and tree, Color by and Aggregation", async () => {
  const p = await pane(twoApps(), { job: { id: "j", state: "failed", error: { code: "index_failed", message: "Index job failed", reason: "internal error" } } });
  try {
    let root = await p.settle();
    const text = () => root.text();
    assert.match(text(), /\[image mods\/trellis\/trellis-mark\.png\] Trellis by SquashMerge/, "the web app's mark and name");
    assert.match(root.find("status-line").text(), /Fresh · rev 96741e4f:1 · Last re-index failed · index_failed: internal error/);
    assert.match(root.find("app-switcher").text(), /^All apps srv · rust webapp · javascript$/);
    assert.match(text(), /2 apps · hidden layers and unassigned files grouped below/);
    assert.ok(root.find("app-header-srv") && root.find("app-header-webapp"), "a header per app column");
    assert.match(root.find("app-header-srv").text(), /2 deviations/);
    assert.match(root.find("interface-card-webapp-srv").text(), /HTTP · 1 route 1 call site 1 exact · 1 unresolved ◆ 1 server-only ◆ 1 client-only/);
    assert.match(root.find("inspector").text(), /Architecture review 2 deviations 0 unassigned files 0 exceptions/);
    assert.match(root.find("legend").text(), /Fixed per-callable bands · cognitive · max · production callables/);
    assert.equal(p.view.drawn.images.length, 2, "900 px stacks the two columns, as the browser does under 880 px of canvas");

    // The card inspects its interface.
    root = await p.click("interface-card-webapp-srv");
    assert.match(root.find("inspector").text(), /webapp → srv .* Client end: webapp · client Server end: srv · server Matched routes 1/);

    // The deviations entry opens the Deviations tree, and a file row opens its functions.
    root = await p.click("review-deviations");
    assert.match(text(), /Deviations 1 file · 3 deviating references · 2 calls/);
    assert.match(root.find("deviation-chips").text(), /All upward · 3 core → server · 3/);
    root = await p.click("deviation-file-0");
    assert.match(root.find("deviation-tree").text(), /src\/util\.rs ▲ server 3 2 3 .* shared/);
    root = await p.click("deviations-show-diagram");
    assert.ok(root.find("diagram"), "Show on diagram returns to the drawing");

    // One app: the switcher re-draws the held snapshot without reading again.
    const reads = p.seen.filter((u) => u.includes("/architecture?")).length;
    root = await p.click("app-1");
    assert.match(text(), /srv · roots \. · Detected from Cargo\.toml/);
    assert.equal(p.view.drawn.images.length, 1);
    assert.match(p.view.drawn.images[0].svg, /srv · ordered layers/);
    assert.equal(p.seen.filter((u) => u.includes("/architecture?")).length, reads);

    // Color by redraws; Aggregation reads the payload again with the new aggregate.
    root = await p.click("control-metric");
    root = await p.click("metric-maxNesting");
    assert.match(root.find("legend").text(), /maxNesting · max/);
    root = await p.click("control-aggregate");
    root = await p.click("aggregate-sum");
    assert.ok(p.seen.some((u) => u.includes("/architecture?aggregate=sum")));
    assert.match(root.find("legend").text(), /Sum · relative totals; no callable risk bands · maxNesting · sum/);
  } finally { await p.close(); }
});

test("the pane, negative control: a one-app checkout shows no switcher, columns or interface card, and no failed-job note", async () => {
  const p = await pane(oneApp(), { job: { id: "j", state: "done", error: null } });
  try {
    const root = await p.settle();
    assert.equal(root.find("app-switcher"), null);
    assert.equal(root.find("app-header-srv"), null);
    assert.doesNotMatch(root.text(), /All apps|HTTP ·|ordered layers/);
    assert.equal(root.find("status-note"), null);
    assert.match(root.find("inspector").text(), /2 deviations/, "the deviations entry stays");
  } finally { await p.close(); }
});

// Review of 604f610b (Prime, #887): runtime reproductions turned into tests.
test("review P2: side by side, reciprocal interface cards with equal counts each name their client → server", async () => {
  const { div } = await import("gpui-kit");
  const ui = await import("../src/architecture-ui.js");
  const v = twoApps(), first = v.interfaces[0];
  v.interfaces.push({ ...structuredClone(first), client: first.server, server: first.client,
    ends: first.ends.map((e) => ({ ...e, clientLayer: e.serverLayer, serverLayer: e.clientLayer })) });
  const out = await draw(v, { width: 1300, theme, highlight: null, choice: {} });
  assert.equal(out.sideBySide, true);
  assert.ok(out.cards.every((c) => c.ends.length === 0), "both are fully routed, so no end words name them");
  const pane = new Trellis();
  Object.assign(pane, { width: 1324, view: v, inspect: null, drawn: { ...out, view: v, assets: ["probe.svg"] } });
  const root = div().children(ui.drawing(pane, theme, pane.drawn));
  const one = root.find("interface-card-webapp-srv").text(), other = root.find("interface-card-srv-webapp").text();
  assert.match(one, /^webapp → srv HTTP · 1 route/);
  assert.match(other, /^srv → webapp HTTP · 1 route/);
  assert.notEqual(one, other);
});

test("review P2: changing Color by drops an inspected label that carries the old metric's value", async () => {
  const view = twoApps(), cx = { notify() {} };
  const pane = new Trellis();
  Object.assign(pane, { view, status: { pin: { indexGeneration: view.indexGeneration, indexRevision: view.indexRevision } }, rootKey: "a",
    open: new Map([["core", true], ["server", true]]), drilled: new Set(), metric: "cognitive", aggregate: "max", scope: "srv", unassignedOpen: false, inspect: null });
  const drawn = await draw(view, { width: 600, theme, highlight: null, choice: pane.choice() });
  pane.drawn = { ...drawn, view, assets: ["unused"] };
  const box = drawn.images[0].boxes.find((b) => b.kind === "M" && b.record.identity.path === "src/main.rs");
  pane.press({ local_position: { x: box.x + box.w / 2, y: box.y + box.h / 2 } }, 0, cx);
  assert.match(pane.inspect.text, /10/, "the label holds the cognitive value");
  pane.choose("metric", "maxNesting", cx);
  const redrawn = await draw(view, { width: 600, theme, highlight: null, choice: pane.choice() });
  pane.drawn = { ...redrawn, view, assets: ["unused"] };
  const { inspector } = await import("../src/architecture-ui.js");
  const text = inspector(pane, theme, redrawn.summary, view)[0].text();
  assert.doesNotMatch(text, /src\/main\.rs/, "no label from the previous metric");
  assert.match(text, /^Architecture review/);
  // Control: re-choosing the same metric keeps the selection.
  pane.press({ local_position: { x: box.x + box.w / 2, y: box.y + box.h / 2 } }, 0, cx);
  pane.choose("metric", "maxNesting", cx);
  assert.equal(pane.inspect?.kind, "label");
});

test("review P3: side by side, the Files card says a visible followed chip is outlined, as stacked does", async () => {
  const view = twoApps(), pane = new Trellis();
  Object.assign(pane, { view, rootKey: "a", metric: "cognitive", aggregate: "max", scope: null, open: new Map(), drilled: new Set(), unassignedOpen: false });
  const file = { path: "src/util.rs", checkout: { rootKey: "a", workspaceRoot: "/work/apps" } };
  for (const width of [600, 1300]) {
    const out = await draw(view, { width, theme, highlight: ["core", "util", "srv"], choice: {} });
    pane.drawn = { ...out, view, assets: out.images.map(() => "unused") };
    assert.match(pane.fileCard(theme, file, pane.placement(file)).text(), /outlined in the diagram/, `width ${width}`);
  }
});

// #913: layer numbers count only the shown layers. With srv's first layer hidden, core is 1, in the
// band label the drawing carries and in the shared label the pane's layer list and Files card use.
test("#913 layer numbers: a hidden first layer takes no number, so the first shown layer is 1", async () => {
  const view = twoApps();
  view.layers = view.layers.map((l) => (l.name === "server" ? { ...l, display: "hidden" } : l));
  const layer = (name) => view.layers.find((l) => l.name === name);
  for (const scope of ["srv", null]) {
    const summary = summarize(view, { scope });
    assert.equal(summary.layerNumber(layer("core")), 1, `core is srv's first shown layer (scope ${scope})`);
    assert.equal(summary.layerNumber(layer("server")), null, "the hidden layer takes no number");
    assert.equal(summary.layerTag(layer("server")), "Hidden");
    if (scope === null) assert.equal(summary.layerNumber(layer("client")), 1, "All apps: each app counts from 1");
    assert.match(summary.layerLabel(layer("core")), /^1 · core/);
  }
  const [image] = (await draw(view, { width: 600, theme, highlight: null, choice: { scope: "srv" } })).images;
  assert.match(image.svg, /1 · core/);
  assert.doesNotMatch(image.svg, /2 · core/);
  // Control: with every layer shown, the numbers are the authored stack's.
  const shown = summarize(twoApps(), { scope: "srv" });
  assert.match(shown.layerLabel(twoApps().layers.find((l) => l.name === "core")), /^2 · core/);
});

// #913: one app's coverage line counts that app's layers; All apps counts the checkout.
test("#913 coverage: one app counts its own layers' reference sites, All apps the checkout's", () => {
  const view = twoApps();
  assert.match(summarize(view).coverageSummary(view), /^4 of 5 reference sites mapped to a module/);
  assert.match(summarize(view, { scope: "srv" }).coverageSummary(view), /^4 of 5 reference sites from srv's layers mapped to a module/);
  assert.match(summarize(view, { scope: "webapp" }).coverageSummary(view), /^0 of 0 reference sites from webapp's layers mapped to a module \(—\)$/);
});

// --- #939: Deviations as a top tab, and the deviation arrows and lines that act ---

// A stand-in for the host's localStorage, where the pane keeps its tab.
function withStorage(entries = {}) {
  const store = new Map(Object.entries(entries)), prior = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, writable: true,
    value: { getItem: (k) => store.get(k) ?? null, setItem: (k, v) => { store.set(k, String(v)); } } });
  return { store, restore() { if (prior) Object.defineProperty(globalThis, "localStorage", prior); else delete globalThis.localStorage; } };
}

test("#939 Deviations is a third top tab: it shows the tree, keeps the choice, and Show on diagram returns", async () => {
  const storage = withStorage();
  const p = await pane(twoApps());
  try {
    let root = await p.settle();
    assert.deepEqual(root.all((el) => /^tab-/.test(el.props.id ?? "")).map((el) => el.text()), ["Architecture", "Sequence", "Deviations"]);
    root = await p.click("tab-deviations");
    assert.equal(p.view.tab, "deviations");
    assert.equal(storage.store.get("trellis.tab"), "deviations", "the tab persists like the others");
    assert.match(root.text(), /Deviations 1 file · 3 deviating references · 2 calls/);
    assert.match(root.find("deviation-chips").text(), /All upward · 3 core → server · 3/, "the edge chips stay in the tab");
    assert.equal(root.find("diagram"), null, "the tab shows the tree, not the drawing");
    assert.equal(root.find("deviations-back"), null, "no back button");
    root = await p.click("deviation-chip-core→server");
    assert.equal(p.view.devs.edge, "core→server");
    root = await p.click("deviations-show-diagram");
    assert.equal(p.view.tab, "architecture");
    assert.equal(storage.store.get("trellis.tab"), "architecture");
    assert.ok(root.find("diagram"));
  } finally { await p.close(); storage.restore(); }
});

test("#939 a kept Deviations tab opens on it and reads the tree without a click", async () => {
  const storage = withStorage({ "trellis.tab": "deviations" });
  const p = await pane(twoApps());
  try {
    const root = await p.settle();
    assert.equal(p.view.tab, "deviations");
    assert.match(root.find("deviation-chips").text(), /All upward/);
    assert.ok(p.seen.some((u) => u.includes("/architecture/details?")));
  } finally { await p.close(); storage.restore(); }
});

test("#939 the review chip, an app's deviation count and a deviating arrow open the Deviations tab with their filter", async () => {
  const storage = withStorage();
  const p = await pane(twoApps());
  try {
    await p.settle();
    let root = await p.click("review-deviations");
    assert.equal(p.view.tab, "deviations");
    assert.equal(p.view.devs.edge, null, "the review chip shows every deviation");
    assert.match(root.find("deviation-chips").text(), /All upward/);

    await p.click("tab-architecture");
    p.cx.stopped = 0;
    root = await p.click("app-deviations-srv");
    assert.equal(p.view.tab, "deviations");
    assert.equal(p.view.scope, "srv", "the app's count filters to that app");
    assert.equal(p.cx.stopped, 1, "the count's press doesn't also inspect the app header");
    assert.ok(p.seen.some((u) => u.includes("/architecture/details?") && u.includes("app=srv")));
    assert.match(root.find("app-switcher").text(), /srv · rust/, "the switcher shows the filter");

    root = await p.click("tab-architecture");
    const boxes = p.view.current().images[0].boxes.filter((b) => b.kind === "E" && b.record.status === "deviates");
    assert.ok(boxes.length >= 2, "a deviating arrow has its label's box and its line's");
    const line = boxes[boxes.length - 1];
    p.view.press({ local_position: { x: line.x + line.w / 2, y: line.y + line.h / 2 } }, 0, p.cx);
    root = await p.settle();
    assert.equal(p.view.tab, "deviations");
    assert.equal(p.view.devs.edge, "core→server", "the arrow filters to its edge");
    assert.equal(root.find("deviation-chip-core→server").text(), "core → server · 3");
  } finally { await p.close(); storage.restore(); }
});

test("#939 a deviation line opens its caller in Sequence; a line with no caller shows its location", async () => {
  const storage = withStorage();
  const p = await pane(twoApps());
  try {
    await p.settle();
    await p.click("review-deviations");
    for (const file of p.view.devs.tree.files) { p.view.devs.expanded.add(file.key); for (const fn of file.functions) p.view.devs.expanded.add(fn.id); }
    let root = await p.settle();
    const lineRow = (start) => root.all((el) => /^deviation-line-/.test(el.props.id ?? "") && el.text().startsWith(`L${start} `))[0];
    // No caller: the location, and the pane's note that it has no source view.
    assert.ok(lineRow(1)?.props.click, "a line row acts");
    lineRow(1).props.click({}, p.cx);
    root = await p.settle();
    assert.equal(p.view.tab, "deviations");
    assert.equal(root.find("deviation-notice").text(), "src/util.rs:1 · This pane has no source view; open it in the browser.");
    // A caller: its sequence, as the browser's Open in Sequence. A file selected in Files keeps
    // its method list, and following it doesn't replace the caller's sequence.
    state.shared["trellis.selectedFile"] = { root: "/work/apps", path: "src/main.rs", on: null };
    lineRow(2).props.click({}, p.cx);
    root = await p.settle();
    assert.equal(p.view.tab, "sequence");
    assert.equal(p.view.seq.file, "src/main.rs");
    assert.equal(p.view.seq.seed.id, "sid:v1:f0f918a5d0e11e7f3ed4125ae69c4db0");
    assert.ok(p.seen.some((u) => u.endsWith("/sequence")), "the caller's sequence is read");
  } finally { await p.close(); storage.restore(); }
});
