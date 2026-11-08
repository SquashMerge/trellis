// The pane end to end in Node: the real src/main.js, with stand-ins for Herdr's modules, against
// a fake loopback daemon, through the exec + curl transport the mod uses for another machine.
//   node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/view.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
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

function fakeDaemon() {
  const seen = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      seen.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
      const send = (status, value) => { res.writeHead(status, { "Content-Type": "application/json" }); res.end(JSON.stringify(value)); };
      if (req.headers.authorization !== `Bearer ${TOKEN}`) return send(401, { error: { code: "unauthorized", message: "Bearer authentication required" } });
      const path = req.url.split("?")[0], base = `/api/checkouts/${KEY}/`;
      if (path === "/api/checkouts") return send(200, { checkouts: [{ rootKey: KEY, state: "available", workspaceRoot: ROOT, revision: pin, catchingUp: false }] });
      if (path === `${base}status`) return send(200, { workspaceRoot: ROOT, revision: pin, catchingUp: false });
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

function mount(props) {
  const view = new Trellis();
  let tasks = 0, last;
  const cx = {
    spawn(fn) { tasks++; Promise.resolve().then(() => fn(cx)).catch((e) => { throw e; }).finally(() => tasks--); return { cancel() {} }; },
    timer: { every() { return { cancel() {} }; }, after(ms, fn) { tasks++; setTimeout(() => { tasks--; fn(cx); }, ms); return { cancel() {} }; } },
    notify() {},
  };
  const settle = async () => {
    for (let quiet = 0, rounds = 0; quiet < 3; rounds++) {
      if (rounds > 300) throw new Error(`the view never settled (${tasks} tasks running)`);
      await new Promise((r) => setTimeout(r, 30)); last = view.render(cx); quiet = tasks ? 0 : quiet + 1;
    }
    return last;
  };
  state.props = props;
  view.init(props, cx);
  return { view, cx, settle, el: () => last };
}

test("Sequence follows the Files selection, draws, and re-centers on a proven call, all through exec + curl", async () => {
  const { server, seen, port } = await fakeDaemon();
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-")), tokenFile = join(dir, "token");
  writeFileSync(tokenFile, `${TOKEN}\n`);
  Object.assign(state, { appHome: dir, focused: { cwd: ROOT }, shared: {}, spaces: [], execs: 0 });
  try {
    const { view, cx, settle } = mount({ url: `http://127.0.0.1:${port}`, tokenFile, transport: "exec" });
    let root = await settle();
    root.find("root").props.size({ width: 900, height: 700 }, cx);
    root = await settle();
    assert.match(root.text(), /No reviewed rules file/, "Architecture shows the daemon's typed state");
    root.find("tab-sequence").props.click({}, cx);
    state.shared["trellis.selectedFile"] = { root: ROOT, path: "fixture.rs", on: null };
    root = await settle();
    assert.match(root.text(), /Methods in fixture\.rs/);
    assert.match(root.text(), /\[image mods\/trellis\/\.render\/sequence-/);
    assert.match(root.text(), /→ helper/, "the proven call names its target");
    assert.ok(state.execs > 0 && seen.every((r) => r.auth === `Bearer ${TOKEN}`));

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
  } finally { server.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("the exec transport types a missing token, a stopped daemon and a rejected token", async () => {
  const { server, port } = await fakeDaemon();
  const dir = mkdtempSync(join(tmpdir(), "trellis-view-"));
  Object.assign(state, { appHome: dir, focused: { cwd: ROOT }, shared: {}, spaces: [] });
  try {
    const run = async (props) => { const { settle } = mount({ transport: "exec", ...props }); return (await settle()).text(); };
    assert.match(await run({ url: `http://127.0.0.1:${port}`, tokenFile: join(dir, "missing") }), /token_missing/);
    writeFileSync(join(dir, "wrong"), "c".repeat(64));
    assert.match(await run({ url: `http://127.0.0.1:${port}`, tokenFile: join(dir, "wrong") }), /unauthorized/);
    writeFileSync(join(dir, "token"), TOKEN);
    server.close();
    const stopped = await run({ url: `http://127.0.0.1:${port}`, tokenFile: join(dir, "token") });
    assert.match(stopped, /daemon_unreachable/);
    assert.ok(!stopped.includes(TOKEN) && !stopped.includes("c".repeat(64)));
  } finally { server.close(); rmSync(dir, { recursive: true, force: true }); }
});
