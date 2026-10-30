"use strict";
const assert = require("node:assert/strict");
const test = require("node:test");
const {existsSync, mkdirSync, realpathSync, rmSync, writeFileSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {fork, spawn} = require("node:child_process");
const {once} = require("node:events");
const H = require("./sequence-harness.cjs");

// Readiness is an IPC hook after index has elected a detached daemon and serve
// has attached. No elapsed-time guess decides when to interrupt the suite.
if (process.env.TRELLIS_SEQUENCE_CLEANUP_CHILD === "1") {
  test.before(() => H.setup());
  test.after(async () => { await H.cleanup(); process.disconnect(); });
  test("owned sequence fixture", async () => {
    const paths = H.fixture();
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env});
    const server = await H.startDaemon(paths, indexed.json().status.revision.indexRevision);
    const listed = await H.run("ps", ["-axww", "-o", "pid=,args="]);
    const commands = new Set([H.binary, realpathSync(H.binary)].map(path => `${path} daemon`));
    const pids = listed.stdout.split("\n").map(line => line.trim().match(/^(\d+) (.*)$/))
      .filter(match => match && commands.has(match[2])).map(match => Number(match[1]));
    assert.equal(pids.length, 1, "fixture elected exactly its own daemon");
    const action = once(process, "message");
    process.send({pid:pids[0], servePid:server.child.pid, binary:H.binary, temp:paths.temp});
    const [message] = await action;
    if (message === "fail") throw new Error("deliberate fixture failure");
  });
} else {
  const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
  let unrelatedPaths;
  test.before(async () => {
    await H.setup();
    unrelatedPaths = H.fixture();
  }, {timeout:960000});
  test.after(async () => {
    await H.cleanup();
    if (unrelatedPaths) rmSync(unrelatedPaths.temp, {recursive:true,force:true,maxRetries:10,retryDelay:100});
  });
  // #475: a child's process exit can be observed before its stdout is read. Here the
  // shell exits first and its background writer finishes stdout afterwards, so a
  // result taken at exit misses output the child wrote; run() keeps all of it.
  test("run returns a child's whole stdout, including output read after its exit", async () => {
    const result = await H.run("/bin/sh", ["-c", "printf '{\"a\":'; (sleep 0.2; printf '1}') & exit 0"]);
    assert.equal(result.stdout, '{"a":1}');
    assert.deepEqual(result.json(), {a:1});
    const truncated = await H.run("/bin/sh", ["-c", "printf '{\"a\":'; echo 'to stderr' >&2"]);
    assert.throws(() => truncated.json(), /sh -c .* exited 0 with unparseable stdout \(5 chars: .*\):\nto stderr\n$/s);
  });

  test("CLI index then serve builds the dependency catalog without explicit refresh", {timeout:150000}, async () => {
    const paths = H.fixture("function measured() { return 1; }\n", "anchor.js");
    let server;
    try {
      const files = {
        "Cargo.toml": "[package]\nname='browser-app'\nversion='0.1.0'\n[dependencies]\nlocal-library={path='vendor/local-library'}\n",
        "Cargo.lock": "version=4\n[[package]]\nname='browser-app'\nversion='0.1.0'\ndependencies=['local-library']\n[[package]]\nname='local-library'\nversion='0.1.0'\n",
        "vendor/local-library/Cargo.toml": "[package]\nname='local-library'\nversion='0.1.0'\n",
        "vendor/local-library/src/lib.rs": "pub fn candidate_only() {}\n",
      };
      for (const [relative, text] of Object.entries(files)) {
        const destination = join(paths.workspace, relative);
        mkdirSync(dirname(destination), {recursive:true});
        writeFileSync(destination, text);
      }
      const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env});
      const pin = indexed.json().status.revision;
      server = await H.startDaemon(paths, pin.indexRevision);
      let catalog;
      const deadline = Date.now() + 120000;
      do {
        const response = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "dependencies"));
        assert.equal(response.status, 200);
        catalog = response.data;
        if (catalog.state !== "loading") break;
        await new Promise(resolve => setTimeout(resolve, 25));
      } while (Date.now() < deadline);
      assert.equal(catalog.state, "ready", JSON.stringify(catalog));
      assert.deepEqual(catalog.workspaceRevision, pin);
      assert.equal(catalog.symbolCount, 1);
      const library = catalog.packages.find(pkg => pkg.name === "local-library");
      assert.ok(library, JSON.stringify(catalog));
      assert.equal(library.indexState, "complete");
      assert.equal(library.sourceState, "present");
      assert.equal(library.source, "workspace-path:vendor/local-library");
    } finally {
      await H.stopDaemon(server);
      rmSync(paths.temp, {recursive:true,force:true,maxRetries:10,retryDelay:100});
    }
  });
  for (const action of ["cleanup", "fail", "SIGTERM", "SIGINT"]) {
    test(`sequence harness cleans its detached daemon on ${action}`, {timeout:150000}, async () => {
      const child = fork(__filename, [], {env:{...process.env,TRELLIS_SEQUENCE_CLEANUP_CHILD:"1"}, stdio:["ignore","pipe","pipe","ipc"]});
      // Same command suffix, different suite binary: never stop this daemon.
      const unrelated = spawn(H.binary, ["daemon"], {env:unrelatedPaths.env,stdio:"ignore"});
      const unrelatedSpawned = once(unrelated, "spawn");
      let stderr = "", owned;
      child.stdout.resume();
      child.stderr.on("data", chunk => { stderr += chunk; });
      const exited = once(child, "exit", {signal:AbortSignal.timeout(120000)});
      try {
        await unrelatedSpawned;
        owned = await Promise.race([
          once(child, "message", {signal:AbortSignal.timeout(120000)}).then(([message]) => message),
          exited.then(([code, exitSignal]) => { throw new Error(`fixture exited before readiness (${code ?? exitSignal}): ${stderr}`); }),
        ]);
        assert.ok(alive(owned.pid), "detached fixture daemon is running before teardown");
        if (action.startsWith("SIG")) child.kill(action); else child.send(action);
        const [code] = await exited;
        assert.equal(alive(owned.pid), false, `test-owned daemon ${owned.pid} leaked after ${action}`);
        assert.equal(alive(owned.servePid), false, "owned serve client is stopped before daemon inventory");
        assert.equal(code, action === "SIGTERM" ? 143 : action === "SIGINT" ? 130 : action === "fail" ? 1 : 0, stderr);
        assert.equal(existsSync(dirname(owned.binary)), false, "suite binary is removed only after daemon exit");
        assert.ok(alive(unrelated.pid), "unrelated process survives fixture cleanup");
      } finally {
        await Promise.all([H.stopDaemon({child:unrelated}), H.stopDaemon({child})]);
        if (owned) {
          if (alive(owned.pid)) process.kill(owned.pid, "SIGTERM");
          if (alive(owned.servePid)) process.kill(owned.servePid, "SIGTERM");
          rmSync(owned.temp, {recursive:true,force:true,maxRetries:10,retryDelay:100});
          rmSync(dirname(owned.binary), {recursive:true,force:true});
        }
      }
    });
  }
}
