"use strict";
const assert = require("node:assert/strict");
const {existsSync, chmodSync, copyFileSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, readdirSync, rmSync, writeFileSync} = require("node:fs");
const {tmpdir} = require("node:os");
const {basename, dirname, join, resolve, sep} = require("node:path");
const {spawn} = require("node:child_process");
const {createHash, randomBytes} = require("node:crypto");
const {createConnection} = require("node:net");
const ROOT = resolve(__dirname,"../..");
const SOURCE_P = "fn seed(value: i32) -> i32 { old_step(value) }\nfn old_step(value: i32) -> i32 { value + 1 }\n";
const scoped = (key,suffix) => `/api/checkouts/${encodeURIComponent(key)}/${suffix}`;
let binary, suiteBinaryDir, cleanupPromise, closing = false;
const clients = new Set();
function ownClient(child) {
  clients.add(child);
  child.once("exit", () => clients.delete(child));
  child.once("error", () => clients.delete(child));
  return child;
}
function interrupted(signal) {
  cleanup().then(() => process.exit(signal === "SIGINT" ? 130 : 143), error => {
    console.error(error);
    process.exit(1);
  });
}
const onInterrupt = () => interrupted("SIGINT");
const onTerminate = () => interrupted("SIGTERM");
const delay = ms => new Promise(resolveDelay => setTimeout(resolveDelay, ms));
// Functional tests assert correctness, not speed: bounds only detect hangs
// (matches trellis::testing::HANG_BACKSTOP, above serve's replacement wait).
const HANG_BACKSTOP_MS = 120000;
function run(command, args, {cwd = ROOT, env = process.env, timeout = 180000} = {}) {
  return new Promise((resolveRun, reject) => {
    if (command === binary && closing) { reject(new Error("sequence suite is closing")); return; }
    const child = spawn(command, args, {cwd, env, stdio:["ignore", "pipe", "pipe"]});
    if (command === binary) ownClient(child);
    let stdout = "", stderr = "", settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), 1000).unref();
      reject(new Error(`${basename(command)} ${args.join(" ")} timed out after ${timeout}ms`));
    }, timeout);
    child.stdout.on("data", chunk => { stdout += chunk; if (stdout.length > 16_000_000) child.kill("SIGTERM"); });
    child.stderr.on("data", chunk => { stderr += chunk; if (stderr.length > 16_000_000) child.kill("SIGTERM"); });
    child.on("error", error => { if (!settled) { settled = true; clearTimeout(timer); reject(error); } });
    // #475: "exit" can fire before the child's stdout has been read, which gave an
    // empty stdout for a successful CLI. "close" fires once the process has exited
    // and its stdio has ended, so the result holds everything the child wrote.
    child.on("close", (code, signal) => {
      if (settled) return;
      settled = true; clearTimeout(timer);
      const label = `${basename(command)} ${args.join(" ")}`;
      // A parse failure names the exit code, the stdout size and stderr.
      const json = () => {
        try { return JSON.parse(stdout); }
        catch (error) { throw new Error(`${label} exited ${code} with unparseable stdout (${stdout.length} chars: ${error.message}):\n${stderr.slice(-16000)}`); }
      };
      if (code === 0) resolveRun({stdout, stderr, code, json});
      else reject(new Error(`${label} failed (${code ?? signal}):\n${stderr.slice(-16000)}`));
    });
  });
}

// tools/verify builds the binary before this suite and names it in
// TRELLIS_SUITE_BINARY; run alone, the suite builds it.
async function builtBinary() {
  const prebuilt = process.env.TRELLIS_SUITE_BINARY;
  if (prebuilt) return resolve(prebuilt);
  const metadata = await run("cargo", ["metadata", "--no-deps", "--format-version", "1"], {timeout:60000});
  const target = metadata.json().target_directory;
  assert.equal(typeof target, "string");
  await run("cargo", ["build", "--locked", "--bin", "trellis"], {timeout:900000});
  return join(target, "debug", process.platform === "win32" ? "trellis.exe" : "trellis");
}
// The CLI `index` each test runs elects a user-level daemon from the suite's
// own binary copy, and that daemon outlives serve by design. The suite stops
// exactly the daemons it started: no other process runs this run-unique
// binary (#130). They are not our children, so exit is observed by signal 0.
async function stopSuiteDaemons() {
  if (!binary) return;
  const commands = new Set([binary, realpathSync(binary)].map(path => `${path} daemon`));
  const listed = await run("ps", ["-axww", "-o", "pid=,args="], {timeout:10000});
  const pids = listed.stdout.split("\n").map(line => line.trim().match(/^(\d+) (.*)$/))
    .filter(match => match && commands.has(match[2])).map(match => Number(match[1]));
  const alive = pid => { try { process.kill(pid,0); return true; } catch { return false; } };
  for (const pid of pids) try { process.kill(pid,"SIGTERM"); } catch {}
  for (const deadline = Date.now()+HANG_BACKSTOP_MS; pids.some(alive); await delay(25)) {
    if (Date.now() > deadline) {
      for (const pid of pids.filter(alive)) try { process.kill(pid,"SIGKILL"); } catch {}
      throw new Error(`suite daemons ignored SIGTERM: ${pids.filter(alive).join(",")}`);
    }
  }
}

function isolatedEnv(paths) {
  const env = {...process.env,
    HOME:paths.home,
    XDG_CACHE_HOME:paths.xdgCache,
    XDG_DATA_HOME:paths.xdgData,
    XDG_CONFIG_HOME:paths.xdgConfig,
    CARGO_HOME:paths.cargoHome,
    RUST_LOG:"trellis=info",
  };
  delete env.RUST_SRC_PATH; delete env.JEV_KEY;
  delete env.TRELLIS_SEARCH_MODEL; delete env.TRELLIS_SEARCH_BACKEND;
  delete env.TRELLIS_INDEX_DIAGNOSTICS;
  return env;
}
function fixture(source = SOURCE_P, filename = "flow.rs") {
  const temp = mkdtempSync(join(tmpdir(), "trellis-saved-browser-"));
  const paths = {temp, workspace:join(temp,"workspace"), home:join(temp,"home"), xdgCache:join(temp,"xdg-cache"),
    xdgData:join(temp,"xdg-data"), xdgConfig:join(temp,"xdg-config"), cargoHome:join(temp,"cargo-home"), secrets:join(temp,"secrets")};
  for (const path of Object.values(paths).slice(1)) mkdirSync(path, {recursive:true, mode:0o700});
  paths.source = join(paths.workspace, filename); paths.tokenFile = join(paths.secrets, "daemon.token"); paths.token = randomBytes(32).toString("hex");
  writeFileSync(paths.source, source); writeFileSync(paths.tokenFile, paths.token, {mode:0o600}); chmodSync(paths.tokenFile,0o600);
  paths.env = isolatedEnv(paths);
  return paths;
}
function findNamed(root, name, found = []) {
  for (const entry of readdirSync(root, {withFileTypes:true})) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) findNamed(path, name, found);
    else if (entry.isFile() && entry.name === name) found.push(path);
  }
  return found;
}
async function listenerDiagnostics(paths) {
  const locks = [...findNamed(paths.home, "daemon.lock"), ...findNamed(paths.xdgData, "daemon.lock")];
  return Promise.all(locks.map(async lock => {
    const dataRoot = dirname(dirname(lock));
    const normal = join(dirname(lock), "daemon.sock");
    const uid = process.getuid();
    const uidBytes = Buffer.alloc(4);
    uidBytes.writeUInt32BE(uid);
    const digest = createHash("sha256").update(Buffer.from(dataRoot)).update(uidBytes).digest("hex").slice(0,32);
    const socketPath = Buffer.byteLength(normal) <= (process.platform === "darwin" ? 103 : 107)
      ? normal : join("/tmp", `trellis-${uid}-${digest}`, "daemon.sock");
    if (!existsSync(socketPath)) return {lock, socketPath, listener:"socket_absent"};
    const listener = await new Promise(resolveListener => {
      const socket = createConnection(socketPath);
      socket.setTimeout(1000);
      socket.once("connect", () => { socket.destroy(); resolveListener("connected"); });
      socket.once("error", error => { socket.destroy(); resolveListener(error.code || error.message); });
      socket.once("timeout", () => { socket.destroy(); resolveListener("timeout"); });
    });
    return {lock, socketPath, listener};
  }));
}
async function startDaemon(paths, expectedRevision) {
  if (closing) throw new Error("sequence suite is closing");
  const child = ownClient(spawn(binary, ["serve", "--workspace", paths.workspace, "--bind", "127.0.0.1:0", "--token-file", paths.tokenFile],
    {cwd:ROOT, env:paths.env, stdio:["ignore","ignore","pipe"]}));
  try {
    let stderr = "", base, rejected;
    const address = new Promise((resolveAddress, rejectAddress) => {
      rejected = rejectAddress;
      child.stderr.on("data", chunk => {
        stderr = (stderr + chunk).slice(-16384);
        const match = stderr.match(/Trellis: (http:\/\/127\.0\.0\.1:\d+)\//);
        if (match && !base) { base = match[1]; resolveAddress(base); }
      });
      child.once("error", rejectAddress);
      child.once("exit", async (code, signal) => {
        if (!base) {
          let listener;
          try { listener = await listenerDiagnostics(paths); }
          catch (error) { listener = `diagnostic_error:${error.code || error.message}`; }
          rejectAddress(new Error(`daemon exited before readiness (${code ?? signal}): listener=${JSON.stringify(listener)} stderr=${stderr.replaceAll(paths.token,"[redacted]")}`));
        }
      });
    });
    // Serve connects to (or launches) the daemon before it prints this banner.
    // The internal connect and reply bounds total 15s; concurrent verify lanes
    // can delay the child beyond the old 10s fixture timer.
    const timer = setTimeout(() => rejected(new Error(`daemon address hung: pid=${child.pid} exit=${child.exitCode} signal=${child.signalCode} stderr=${stderr.replaceAll(paths.token,"[redacted]")}`)),HANG_BACKSTOP_MS);
    try { await address; } finally { clearTimeout(timer); }
    const deadline = Date.now()+HANG_BACKSTOP_MS;
    while (true) {
      try { const health=await fetch(`${base}/healthz`,{signal:AbortSignal.timeout(1000)}); if(health.ok) break; } catch {}
      if(Date.now()>deadline) throw new Error(`daemon health timed out: ${stderr.replaceAll(paths.token,"[redacted]")}`);
      await delay(20);
    }
    const selected = {child, base, stderr:() => stderr};
    const discoveryDeadline = Date.now()+25000;
    let lastBusyRow = null;
    while (Date.now() < discoveryDeadline) {
      const listing = await api(base,paths.token,"GET","/api/checkouts");
      assert.equal(listing.status,200,JSON.stringify(listing.data));
      const rows = listing.data.checkouts.filter(row => row.workspaceRoot === realpathSync(paths.workspace));
      assert.equal(rows.length,1,`expected exactly one matching workspace: ${JSON.stringify(listing.data)}`);
      const row = rows[0];
      assert.ok(typeof row.rootKey === "string" && /^[a-f0-9]{64}$/.test(row.rootKey),`invalid checkout key: ${JSON.stringify(row)}`);
      if (row.state === "storage_busy") {
        lastBusyRow = row;
        await delay(50);
        continue;
      }
      assert.equal(row.state,"available",`checkout is not selectable: ${JSON.stringify(row)}`);
      lastBusyRow = null;
      selected.rootKey = row.rootKey;
      const status = await api(base,paths.token,"GET",scoped(selected.rootKey,"status"));
      if (status.status === 200) {
        if (status.data.revision?.indexRevision === expectedRevision) return selected;
        assert.ok(status.data.revision?.indexRevision < expectedRevision, `unexpected startup revision: ${JSON.stringify(status.data.revision)}`);
        await delay(50); continue;
      }
      assert.equal(status.status,503,JSON.stringify(status.data));
      await delay(50);
    }
    throw new Error(`selected checkout did not become ready within 25s${lastBusyRow ? `: persistent ${JSON.stringify(lastBusyRow)}` : ""}: ${stderr.replaceAll(paths.token,"[redacted]")}`);
  } catch (error) { child.kill("SIGTERM"); throw error; }
}
async function stopDaemon(server) {
  if (!server?.child || server.child.exitCode !== null || server.child.signalCode !== null) return;
  server.child.kill("SIGTERM");
  const exited = new Promise(resolveExit => server.child.once("exit",resolveExit));
  if (await Promise.race([exited.then(()=>true),delay(3000).then(()=>false)])) return;
  server.child.kill("SIGKILL");
  if (!await Promise.race([exited.then(()=>true),delay(2000).then(()=>false)])) {
    throw new Error(`suite client ${server.child.pid} did not exit after SIGKILL`);
  }
}
async function api(base, token, method, route, body) {
  const response = await fetch(`${base}${route}`, {method, signal:AbortSignal.timeout(8000),
    headers:{Origin:base,Authorization:`Bearer ${token}`,...(body === undefined ? {} : {"Content-Type":"application/json"})},
    body:body === undefined ? undefined : JSON.stringify(body)});
  const data = response.status === 204 ? null : await response.json();
  return {status:response.status,data};
}
const pinSuffix = pin => `?indexGeneration=${encodeURIComponent(pin.indexGeneration)}&indexRevision=${pin.indexRevision}`;
function assertPinnedUrl(raw, pin) {
  const url = new URL(raw);
  assert.equal(url.searchParams.get("indexGeneration"),pin.indexGeneration);
  assert.equal(url.searchParams.get("indexRevision"),String(pin.indexRevision));
}
async function indexThroughApi(server, token, expected, changedPath, changedSource) {
  const accepted=await api(server.base,token,"POST",scoped(server.rootKey,"index"),{expectedRevision:expected});assert.equal(accepted.status,202,JSON.stringify(accepted.data));
  const deadline=Date.now()+HANG_BACKSTOP_MS;
  let job;
  while(Date.now()<deadline){
    job=await api(server.base,token,"GET",scoped(server.rootKey,`jobs/${encodeURIComponent(accepted.data.id)}`));
    assert.equal(job.status,200,JSON.stringify(job.data));
    if(job.data.finishedAt!==null)break;
    await delay(25);
  }
  if(job?.data.finishedAt===null||job===undefined)throw new Error("index job did not complete within the hang backstop");
  // A finished job can briefly leave the checkout catching up or contended.
  let current;
  while(true){
    current=await api(server.base,token,"GET",scoped(server.rootKey,"status"));
    if(current.status===503&&["storage_busy","index_not_ready"].includes(current.data?.error?.code)&&Date.now()<deadline){await delay(25);continue;}
    break;
  }
  assert.equal(current.status,200,JSON.stringify(current.data));
  let pin;
  if(job.data.state==="done"){
    pin=job.data.revision;
    assert.deepEqual(current.data.revision,pin,"completed job must select its returned pin");
  }else{
    // The watcher can publish the edited source first (#131). The stale explicit
    // job must stay failed, and the selected newer pin must hold the edited bytes.
    assert.equal(job.data.error?.code,"revision_conflict",JSON.stringify(job.data));
    pin=current.data.revision;
  }
  assert.equal(pin.indexGeneration,expected.indexGeneration);
  assert.ok(pin.indexRevision>expected.indexRevision,`no newer selected head: ${JSON.stringify(job.data)}; current=${JSON.stringify(current.data)}`);
  let source;
  while(true){
    source=await api(server.base,token,"GET",scoped(server.rootKey,`source${pinSuffix(pin)}&path=${encodeURIComponent(changedPath)}`));
    if(source.status===503&&["storage_busy","index_not_ready"].includes(source.data?.error?.code)&&Date.now()<deadline){await delay(25);continue;}
    break;
  }
  assert.equal(source.status,200,JSON.stringify(source.data));
  assert.equal(source.data.file.text,changedSource,"attested changed source at selected pin");
  return pin;
}
async function seedId(server, token, name) {
  const files=await api(server.base,token,"GET",scoped(server.rootKey,"files?offset=0&limit=200"));assert.equal(files.status,200,JSON.stringify(files.data));
  for (const file of files.data.items) {
    const methods=await api(server.base,token,"GET",scoped(server.rootKey,`methods?path=${encodeURIComponent(file.path)}`));assert.equal(methods.status,200,JSON.stringify(methods.data));
    const item=methods.data.items.find(value=>value.symbol.name===name);if(item)return item.symbol.id;
  }
  assert.fail(`missing ${name} method`);
}
async function connect(page, server, token) {
  const {base,rootKey} = server;
  page.checkoutRootKey = rootKey;
  page.serveLog = () => server.stderr().replaceAll(token,"[redacted]").slice(-4000);
  await page.goto(`${base}/`,{waitUntil:"domcontentloaded"});
  await page.locator("#token").fill(token);
  await page.locator("#connect-form button[type=submit]").click();
  await page.locator("#checkout-select").waitFor({state:"visible",timeout:15000});
  await page.locator("#checkout-select").selectOption(rootKey);
  await page.locator("#workspace").waitFor({state:"visible",timeout:15000});
  await page.waitForFunction(() => document.querySelector("#status")?.textContent?.includes("Revision"),null,{timeout:15000});
}
async function setup() {
  const built = await builtBinary();
  suiteBinaryDir = mkdtempSync(join(tmpdir(),"trellis-sequence-binary-"));
  binary = join(suiteBinaryDir,basename(built)); copyFileSync(built,binary);
  process.on("SIGINT", onInterrupt);
  process.on("SIGTERM", onTerminate);
}
function cleanup() {
  if (!cleanupPromise) {
    closing = true;
    cleanupPromise = (async () => {
      // Stop and reap clients before the daemon inventory: an in-flight CLI
      // must not elect another detached daemon after that inventory.
      await Promise.all([...clients].map(child => stopDaemon({child})));
      await stopSuiteDaemons();
      if (suiteBinaryDir) rmSync(suiteBinaryDir,{recursive:true,force:true});
      process.removeListener("SIGINT", onInterrupt);
      process.removeListener("SIGTERM", onTerminate);
    })();
  }
  return cleanupPromise;
}
module.exports = {setup,cleanup,fixture,run,startDaemon,stopDaemon,api,scoped,connect,seedId,indexThroughApi,chromiumPath:undefined,get binary(){return binary;}};
