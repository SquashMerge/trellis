// The pane end to end in Node: the real src/main.js, with stand-ins for Herdr's modules, against
// a fake loopback daemon, through the `trellis api` exec transport the mod uses for another
// machine. A fake `trellis` stands in for the CLI: it relays to the fake daemon and prints and
// exits as `trellis api` does (#817).
//   node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/view.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { join } from "node:path";
import { state } from "./stubs/herdr.js";
import Trellis from "../src/main.js";

const ROOT = "/work/repo", KEY = "a".repeat(64), TOKEN = "b".repeat(64);
const pin = { indexGeneration: "00000000-0000-4000-8000-000000000001", indexRevision: 7 };
const fixture = () => JSON.parse(readFileSync(new URL("../../tests/fixtures/sequence-pele/rust.json", import.meta.url), "utf8"));

// The seed's one call is proven to reach `helper` (the browser's own proof rule).
function sequenceFor(seed) {
  const view = fixture();
  if (seed === "helper") {
    view.seed = { ...view.seed, id: "helper", name: "helper" };
    view.participants[0] = { ...view.participants[0], id: "helper", label: "helper" };
    return view;
  }
  const target = { id: "helper", name: "helper", kind: "function", path: "fixture.rs", range: { startLine: 2, endLine: 4 } };
  view.participants.push({ id: "helper", label: "helper", kind: "internal", declaration: target });
  view.steps = [{ ...view.steps[0], id: "call-helper", callId: "occ-1", label: "helper()", target: "helper", hidden: false, children: [], alternate: [],
    callEvidence: { boundaryReason: "none", resolution: "resolved", dispatch: "direct", freshness: "fresh", provenanceId: "proof" } }];
  return view;
}

// The browser's own Architecture fixture, as the daemon serves a ready answer.
const architecture = () => {
  const { fixture: make } = createRequire(import.meta.url)("../../tests/fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
  const v = make();
  for (const m of v.modules) m.declarationCount = 1;
  return { ...v, ...pin, aggregate: "max", role: "production", workbench: true, metricsVersion: "metrics:v1", evidenceTier: "syntax",
    semanticComplete: false, rulesFile: { contentHash: "abc" }, references: { coverage: [] } };
};

function fakeDaemon({ ready = false, checkouts = [{ key: KEY, root: ROOT }] } = {}) {
  const seen = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      seen.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
      const send = (status, value) => { res.writeHead(status, { "Content-Type": "application/json" }); res.end(JSON.stringify(value)); };
      if (req.headers.authorization !== `Bearer ${TOKEN}`) return send(401, { error: { code: "unauthorized", message: "Bearer authentication required" } });
      const path = req.url.split("?")[0];
      if (path === "/api/checkouts") return send(200, { checkouts: checkouts.map((c) => ({ rootKey: c.key, state: "available", workspaceRoot: c.root, revision: pin, catchingUp: false })) });
      const listed = checkouts.find((c) => path.startsWith(`/api/checkouts/${c.key}/`));
      if (!listed) return send(404, { error: { code: "not_found", message: "Not found" } });
      const base = `/api/checkouts/${listed.key}/`;
      if (path === `${base}status`) return send(200, { workspaceRoot: listed.root, revision: pin, catchingUp: false });
      if (path === `${base}architecture` && ready) return send(200, { payload: architecture() });
      if (path === `${base}architecture`) return send(200, { payload: { ...pin, state: "missing_rules", aggregate: "max", role: "production", workbench: true,
        metricsVersion: "metrics:v1", evidenceTier: "syntax", semanticComplete: false, layers: [], modules: [], edges: [] } });
      if (path === `${base}methods`) return send(200, { items: [{ consequential: true, symbol: { id: "fixture-seed", name: "run", kind: "function", path: "fixture.rs" } }] });
      if (path === `${base}sequence` && req.method === "POST") {
        const { seed, expectedRevision } = JSON.parse(body);
        assert.deepEqual(expectedRevision, pin);
        return send(200, sequenceFor(seed));
      }
      send(404, { error: { code: "not_found", message: "No such route" } });
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, seen, port: server.address().port })));
}

// `trellis api` as the CLI behaves: exit 0 with the daemon's 2xx JSON, exit 1 with its {error}
// answer unchanged, exit 2 with a typed {error} when no daemon answers. `mode` "timeout" stands
// for exec's own timeout, which rejects.
function fakeTrellis(port, { mode = "ok" } = {}) {
  const calls = [];
  state.exec = async (command, args, options) => {
    calls.push({ command, args, on: options?.on ?? null, timeout: options?.timeout });
    if (command !== "trellis" || args[0] !== "api") return { code: 127, stdout: "", stderr: "not found" };
    if (mode === "timeout") throw new Error("exec timed out after 30000 ms");
    if (mode === "unreachable") return { code: 2, stdout: '{"error":{"code":"daemon_unavailable","message":"The daemon call failed (daemon_unavailable)."}}\n', stderr: "" };
    const [, method, target, flag, body] = args;
    assert.ok(method === "GET" ? args.length === 3 : flag === "--body", `argv shape: ${args}`);
    const response = await fetch(`http://127.0.0.1:${port}${target}`, method === "GET" ? { headers: { Authorization: `Bearer ${TOKEN}` } }
      : { method, headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" }, body });
    const text = await response.text();
    return { code: response.ok ? 0 : 1, stdout: `${text}\n`, stderr: "" };
  };
  return calls;
}

// `hang` stands for a task the Herdr runtime ended over its synchronous-run cap: the drawing's
// first yield never returns, and the watchdog's 15 s are 40 ms.
function mount(props, { hang = false } = {}) {
  const view = new Trellis();
  let tasks = 0, last;
  const cx = {
    spawn(fn) { tasks++; Promise.resolve().then(() => fn(cx)).catch((e) => { throw e; }).finally(() => tasks--); return { cancel() {} }; },
    timer: { every() { return { cancel() {} }; }, after(ms, fn) {
      tasks++; let done = false; const end = () => { if (!done) { done = true; tasks--; } };
      const id = setTimeout(() => { end(); fn(cx); }, hang && ms >= 15000 ? 40 : ms); return { cancel() { clearTimeout(id); end(); } };
    } },
    sleep(ms = 0) { return hang ? new Promise(() => {}) : new Promise((r) => setTimeout(r, ms)); },
    notify() {},
  };
  const settle = async () => {
    for (let quiet = 0, rounds = 0; quiet < 3; rounds++) {
      if (rounds > 300) throw new Error(`the view never settled (${tasks} tasks running)`);
      await new Promise((r) => setTimeout(r, 30)); last = view.render(cx); quiet = tasks ? 0 : quiet + 1;
    }
    return last;
  };
  // Renders until the pane shows `pattern` (a hung task never lets the view settle).
  const until = async (pattern) => {
    for (let rounds = 0; rounds < 200; rounds++) {
      await new Promise((r) => setTimeout(r, 20)); last = view.render(cx);
      if (pattern.test(last.text())) return last;
    }
    throw new Error(`the pane never showed ${pattern}: ${last.text().slice(0, 400)}`);
  };
  state.props = props;
  view.init(props, cx);
  return { view, cx, settle, until, el: () => last };
}

test("Sequence follows the Files selection, draws, and re-centers on a proven call, all through trellis api on the space's machine", async () => {
  const { server, seen, port } = await fakeDaemon();
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-"));
  Object.assign(state, { appHome: dir, focused: { cwd: ROOT }, shared: {}, spaces: [{ active: true, machine: "mini" }], machines: [{ name: "mini", layout: "ready" }], execs: 0 });
  const calls = fakeTrellis(port);
  try {
    const { view, cx, settle } = mount({});
    let root = await settle();
    root.find("root").props.size({ width: 900, height: 700 }, cx);
    root = await settle();
    assert.match(root.text(), /No reviewed rules file/, "Architecture shows the daemon's typed state");
    root.find("tab-sequence").props.click({}, cx);
    state.shared["trellis.selectedFile"] = { root: ROOT, path: "fixture.rs", on: "mini" };
    root = await settle();
    assert.match(root.text(), /Methods in fixture\.rs/);
    assert.match(root.text(), /\[image mods\/trellis\/\.render\/sequence-/);
    assert.match(root.text(), /→ helper/, "the proven call names its target");
    assert.ok(calls.length > 0 && calls.every((c) => c.command === "trellis" && c.on === "mini" && c.timeout === 30000));
    assert.ok(calls.every((c) => !c.args.join(" ").includes(TOKEN)), "no token in the command");
    assert.ok(calls.some((c) => c.args[1] === "POST" && c.args[3] === "--body" && JSON.parse(c.args[4]).seed === "fixture-seed"));

    // A press on the call's drawn step re-centers on the target, with Back to the seed.
    const box = view.seq.drawn.boxes.find((b) => b.step === "call-helper");
    root.find("sequence-diagram").props["press-left"]({ local_position: { x: box.x + 2, y: box.y + 2 } }, cx);
    root = await settle();
    assert.equal(view.seq.seed.id, "helper");
    assert.equal(JSON.parse(seen.filter((r) => r.url.endsWith("/sequence")).at(-1).body).seed, "helper");
    root.find("sequence-back").props.click({}, cx);
    root = await settle();
    assert.equal(view.seq.seed.id, "fixture-seed");
    await view.close();
  } finally { delete state.exec; state.spaces = []; delete state.machines; server.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("trellis api failures are typed: a daemon error keeps its code, an unreachable daemon, a timeout, no trellis", async () => {
  const { server, port } = await fakeDaemon();
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-"));
  Object.assign(state, { appHome: dir, focused: { cwd: ROOT }, shared: {}, spaces: [] });
  const run = async (mode, props = {}) => {
    fakeTrellis(port, { mode });
    const { view, settle } = mount({ transport: "exec", ...props });
    const text = (await settle()).text();
    await view.close();
    return text;
  };
  try {
    // A daemon error answer (exit 1) is shown with the daemon's own code.
    state.focused = { pane: "p", terminal: "t", cwd: ROOT };
    fakeTrellis(port);
    const failing = state.exec;
    state.exec = (command, args, options) => args[2].endsWith("/status")
      ? Promise.resolve({ code: 1, stdout: '{"error":{"code":"index_not_ready","message":"reindex required"}}\n', stderr: "" }) : failing(command, args, options);
    let { view, settle } = mount({ transport: "exec" });
    assert.match((await settle()).text(), /Index not ready \(index_not_ready: reindex required\)/);
    await view.close();
    assert.match(await run("unreachable"), /daemon_unavailable/);
    const timeout = await run("timeout");
    assert.match(timeout, /daemon_timeout/);
    assert.match(timeout, /did not answer within 30 s/);
    // No `trellis` on that machine's PATH.
    state.exec = async () => ({ code: 127, stdout: "", stderr: "trellis: command not found" });
    ({ view, settle } = mount({ transport: "exec" }));
    assert.match((await settle()).text(), /trellis_missing/);
    await view.close();
    // An older trellis without the `api` command.
    state.exec = async () => ({ code: 2, stdout: "", stderr: "error: unrecognized subcommand 'api'" });
    ({ view, settle } = mount({ transport: "exec" }));
    assert.match((await settle()).text(), /trellis_outdated/);
    await view.close();
  } finally { delete state.exec; server.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("a drawing the runtime ends is a typed timeout, not Drawing… for ever, in both views", async () => {
  const { server, port } = await fakeDaemon({ ready: true });
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-")), tokenFile = join(dir, "token");
  writeFileSync(tokenFile, `${TOKEN}\n`);
  Object.assign(state, { appHome: dir, focused: { cwd: ROOT }, shared: {}, spaces: [] });
  fakeTrellis(port);
  try {
    const { view, cx, until } = mount({ url: `http://127.0.0.1:${port}`, tokenFile, transport: "exec" }, { hang: true });
    let root = await until(/Drawing the layer stack/);
    root.find("root").props.size({ width: 900, height: 700 }, cx);
    root = await until(/draw_timeout/);
    assert.match(root.text(), /Architecture drawing didn't finish within 15 s/);
    assert.equal(view.drawing, false, "the watchdog frees the drawing slot");
    root.find("tab-sequence").props.click({}, cx);
    state.shared["trellis.selectedFile"] = { root: ROOT, path: "fixture.rs", on: null };
    root = await until(/Sequence drawing didn't finish/);
    assert.match(root.text(), /draw_timeout/);
    assert.equal(view.seqDrawing, false);
    await view.close();
  } finally { delete state.exec; server.close(); rmSync(dir, { recursive: true, force: true }); }
});

// #809: the pane follows the focused agent's checkout; a mod pane's focus (cwd null) keeps it;
// the pin stops it; a later Files selection wins; a folder in no listed checkout is a note.
const KEY_B = "c".repeat(64), ROOT_B = "/work/other";
const agentIn = (pane, cwd) => ({ space: "s1", tab: "t1", pane, terminal: `term-${pane}`, shown: null, agent: "claude", cwd });
const modPane = (pane) => ({ space: "s1", tab: "t1", pane, terminal: null, shown: null, agent: null, cwd: null });

async function following(fn) {
  const { server, seen, port } = await fakeDaemon({ checkouts: [{ key: KEY, root: ROOT }, { key: KEY_B, root: ROOT_B }] });
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-")), tokenFile = join(dir, "token");
  writeFileSync(tokenFile, `${TOKEN}\n`);
  Object.assign(state, { appHome: dir, focused: agentIn("p1", `${ROOT}/src`), shared: {}, spaces: [] });
  fakeTrellis(port);
  try {
    const pane = mount({ url: `http://127.0.0.1:${port}`, tokenFile, transport: "exec" });
    await pane.settle();
    await fn({ ...pane, seen, statusReads: (key) => seen.filter((r) => r.url === `/api/checkouts/${key}/status`).length });
    await pane.view.close();
  } finally { delete state.exec; server.close(); rmSync(dir, { recursive: true, force: true }); }
}

test("the pane follows the focused agent from one checkout to another, and reads only those", async () => {
  await following(async ({ view, settle, statusReads }) => {
    assert.equal(view.rootKey, KEY, "agent A's folder is inside the first checkout");
    assert.equal(statusReads(KEY_B), 0, "a checkout nobody points at is never read");
    state.focused = agentIn("p2", ROOT_B);
    const root = await settle();
    assert.equal(view.rootKey, KEY_B);
    assert.match(root.find("checkout-label").text(), /other/);
    assert.equal(root.find("checkout-button"), null, "no checkout dropdown");
    assert.ok(statusReads(KEY_B) > 0);
  });
});

test("focusing a mod pane, this one included, keeps the last agent's checkout", async () => {
  await following(async ({ view, settle }) => {
    state.focused = agentIn("p2", `${ROOT_B}/deep/folder`);
    await settle();
    assert.equal(view.rootKey, KEY_B);
    state.focused = modPane("trellis-pane");
    await settle();
    assert.equal(view.rootKey, KEY_B);
    state.focused = modPane("files-pane");
    await settle();
    assert.equal(view.rootKey, KEY_B);
  });
});

test("pinned, the pane stays on its checkout; unpinned, it follows the focused agent again", async () => {
  await following(async ({ view, cx, settle }) => {
    let root = await settle();
    root.find("pin").props.click({}, cx);
    state.focused = agentIn("p2", ROOT_B);
    root = await settle();
    assert.equal(view.rootKey, KEY, "pinned: no follow");
    assert.match(root.find("pin").text(), /Pinned/);
    root.find("pin").props.click({}, cx);
    root = await settle();
    assert.equal(view.rootKey, KEY_B, "unpinned: follows the agent focused now");
    assert.match(root.find("pin").text(), /Following/);
  });
});

test("a Files selection after an agent focus wins, and a later focus wins back", async () => {
  await following(async ({ view, settle }) => {
    assert.equal(view.rootKey, KEY);
    state.shared["trellis.selectedFile"] = { root: ROOT_B, path: "lib.rs", on: null };
    await settle();
    assert.equal(view.rootKey, KEY_B, "the more recent event is the selection");
    state.focused = modPane("files-pane");
    await settle();
    assert.equal(view.rootKey, KEY_B, "the Files pane's own focus changes nothing");
    state.focused = agentIn("p1", `${ROOT}/src`);
    await settle();
    assert.equal(view.rootKey, KEY, "focusing the agent again is now the more recent event");
  });
});

test("a folder in no checkout the daemon lists is a note naming it, with no guess", async () => {
  await following(async ({ view, settle, seen }) => {
    const reads = seen.length;
    state.focused = agentIn("p3", "/Users/me/fresh-clone");
    const root = await settle();
    assert.equal(view.rootKey, null);
    assert.match(root.text(), /\/Users\/me\/fresh-clone isn't in a checkout this daemon lists/);
    assert.match(root.find("checkout-label").text(), /fresh-clone/);
    assert.ok(seen.slice(reads).every((r) => !r.url.includes("/status")), "nothing is read for an unknown folder");
  });
});

// #819 review: stale completions. Each async read owns its request; a result, rejection or
// `finally` of a request that is no longer current changes nothing. Deferred promises, no timing.
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
function bare() {
  const jobs = [];
  const cx = { spawn(fn) { jobs.push(fn(cx)); return { cancel() {} }; }, notify() {}, timer: { after() { return { cancel() {} }; }, every() { return { cancel() {} }; } } };
  const view = new Trellis();
  view.props = {}; view.events = 0;
  return { view, cx, jobs, last: () => jobs.at(-1) };
}
const KEY_A = "a".repeat(64), KEY_C = "c".repeat(64);
const pin2 = { ...pin, indexRevision: 8 };
const rulesless = (p) => ({ payload: { ...p, state: "missing_rules", aggregate: "max", role: "production", workbench: true, metricsVersion: "metrics:v1",
  evidenceTier: "syntax", semanticComplete: false, layers: [], modules: [], edges: [] } });

test("P2-1: a status read obsoleted by a checkout switch frees nothing of B's, and B issues and shows its own", async () => {
  const { view, cx, last } = bare();
  const reads = { [KEY_A]: deferred(), [KEY_C]: deferred() }, calls = [];
  view.client = { status: (key) => { calls.push(key); return reads[key].promise; }, architecture: () => new Promise(() => {}) };
  view.select(KEY_A, cx); const jobA = last();
  view.select(KEY_C, cx); const jobC = last();
  assert.deepEqual(calls, [KEY_A, KEY_C], "B's refresh sent its own request");
  reads[KEY_A].resolve({ revision: pin, workspaceRoot: "/a" }); await jobA;
  assert.equal(view.status, null, "A's answer is dropped");
  assert.equal(view.statusBusy, true, "B's read is still the one in flight");
  reads[KEY_C].resolve({ revision: pin2, workspaceRoot: "/c" }); await jobC;
  assert.deepEqual(view.status.pin, pin2);
  assert.equal(view.statusBusy, false);
  view.refreshStatus(cx);
  assert.equal(calls.at(-1), KEY_C, "later polls still read B");
});

test("P2-1b: a status rejection obsoleted by a switch sets no error on B", async () => {
  const { view, cx, last } = bare();
  const a = deferred();
  view.client = { status: (key) => key === KEY_A ? a.promise : Promise.resolve({ revision: pin2 }), architecture: () => new Promise(() => {}) };
  view.select(KEY_A, cx); const jobA = last();
  view.select(KEY_C, cx); await last();
  a.reject(new TrellisErrorLike("daemon_unreachable")); await jobA;
  assert.equal(view.statusError, null);
  assert.deepEqual(view.status.pin, pin2);
});

test("P2-2: sequence B resolves, then the obsolete A rejects: B stays shown with no error", async () => {
  const { view, cx, jobs } = bare();
  view.rootKey = KEY_A; view.serial = 1; view.status = { pin }; view.seq = { history: [], expanded: new Set() };
  const a = deferred(), b = deferred();
  view.client = { sequence: (_k, _p, id) => (id === "A" ? a.promise : b.promise) };
  view.loadSequence({ id: "A", name: "alpha" }, false, cx); const jobA = jobs.at(-1);
  view.loadSequence({ id: "B", name: "beta" }, false, cx); const jobB = jobs.at(-1);
  b.resolve({ revision: pin, seed: { id: "B", name: "beta" }, steps: [], participants: [] }); await jobB;
  a.reject(new Error("old A request failed")); await jobA;
  assert.equal(view.seq.data.seed.id, "B");
  assert.equal(view.seq.error, null);
  assert.equal(view.seq.loading, false);
});

test("P2-2b: an obsolete sequence that resolves first neither shows its data nor ends the successor's loading", async () => {
  const { view, cx, jobs } = bare();
  view.rootKey = KEY_A; view.serial = 1; view.status = { pin }; view.seq = { history: [], expanded: new Set() };
  const a = deferred(), b = deferred();
  view.client = { sequence: (_k, _p, id) => (id === "A" ? a.promise : b.promise) };
  view.loadSequence({ id: "A", name: "alpha" }, false, cx); const jobA = jobs.at(-1);
  view.loadSequence({ id: "B", name: "beta" }, false, cx);
  a.resolve({ revision: pin, seed: { id: "A", name: "alpha" }, steps: [], participants: [] }); await jobA;
  assert.equal(view.seq.data, null);
  assert.equal(view.seq.loading, true, "B is still loading");
  // A, then B, then A again: the first A's answer is not the current A request's.
  view.loadSequence({ id: "A", name: "alpha" }, false, cx);
  assert.equal(view.seq.data, null);
});

test("P2-3: an obsolete Architecture read for rev 7 rejecting after rev 8 succeeded keeps rev 8's view", async () => {
  const { view, cx, jobs } = bare();
  view.rootKey = KEY_A; view.serial = 1; view.status = { pin };
  const old = deferred(), current = deferred();
  view.client = { architecture: (_k, p) => (p.indexRevision === 7 ? old.promise : current.promise), status: () => Promise.resolve({ revision: pin2 }) };
  view.loadArchitecture(cx); const jobOld = jobs.at(-1);
  view.status = { pin: pin2 }; view.loadArchitecture(cx); const jobCurrent = jobs.at(-1);
  current.resolve(rulesless(pin2)); await jobCurrent;
  old.reject(new Error("old revision architecture failed")); await jobOld;
  assert.equal(view.view.state, "missing_rules");
  assert.equal(view.viewError, null);
  assert.equal(view.loading, false);
});

test("an obsolete checkout list that answers last is dropped", async () => {
  const { view, cx, jobs } = bare();
  const first = deferred(), second = deferred(); let n = 0;
  view.client = { checkouts: () => (n++ === 0 ? first.promise : second.promise) };
  view.loadCheckouts(cx); const jobFirst = jobs.at(-1);
  view.loadCheckouts(cx); const jobSecond = jobs.at(-1);
  second.resolve({ checkouts: [{ rootKey: KEY_C, state: "available", workspaceRoot: "/c" }] }); await jobSecond;
  first.reject(new Error("old list failed")); await jobFirst;
  assert.equal(view.listError, null);
  assert.equal(view.checkouts[0].rootKey, KEY_C);
});

test("an obsolete method list's rejection sets no error on the reloaded file", async () => {
  const { view, cx, jobs } = bare();
  view.rootKey = KEY_A; view.serial = 1; view.status = { pin }; view.seq = { file: "f.rs", history: [], expanded: new Set() };
  const old = deferred(), current = deferred();
  let n = 0;
  view.client = { methods: () => (n++ === 0 ? old.promise : current.promise), sequence: () => new Promise(() => {}) };
  view.loadMethods(cx); const jobOld = jobs.at(-1);
  view.reloadSequence(cx); const jobCurrent = jobs.at(-1);
  current.resolve({ items: [{ consequential: true, symbol: { id: "s", name: "run" } }] }); await jobCurrent;
  old.reject(new Error("old methods failed")); await jobOld;
  assert.equal(view.seq.methodsError, null);
  assert.equal(view.seq.methods[0].id, "s");
});

function TrellisErrorLike(code) { const e = new Error(code); e.code = code; return e; }
