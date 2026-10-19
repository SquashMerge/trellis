"use strict";
const assert = require("node:assert/strict");
const test = require("node:test");
const { existsSync, chmodSync, copyFileSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, readdirSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { basename, dirname, join, resolve, sep } = require("node:path");
const { spawn } = require("node:child_process");
const { createHash, randomBytes } = require("node:crypto");
const { createConnection } = require("node:net");
const { DatabaseSync } = require("node:sqlite");

const browserRoot = process.env.PLAYWRIGHT_BROWSERS_PATH;
assert.ok(browserRoot, "PLAYWRIGHT_BROWSERS_PATH must point to the run-local Playwright browser directory");
const { chromium } = require("playwright");
const chromiumPath = resolve(chromium.executablePath());
assert.ok(existsSync(chromiumPath), `Playwright Chromium is unavailable at ${chromiumPath}`);
assert.ok(chromiumPath.startsWith(`${resolve(browserRoot)}${sep}`), "Chromium must come from PLAYWRIGHT_BROWSERS_PATH");

const ROOT = resolve(__dirname, "../..");
const SOURCE_P = "fn seed(value: i32) -> i32 { old_step(value) }\nfn old_step(value: i32) -> i32 { value + 1 }\nfn other() -> i32 { 0 }\n";
const SOURCE_Q = "fn seed(value: i64) -> i64 { new_step(value) }\nfn new_step(value: i64) -> i64 { value + 2 }\nfn other() -> i32 { 0 }\n";
// The production listener requires an explicit root key for every scoped API call.
const scoped = (key, suffix) => `/api/checkouts/${encodeURIComponent(key)}/${suffix}`;
const browserPath = (page, suffix) => scoped(page.checkoutRootKey, suffix);
function assertSelectedBrowserRequests(requests, rootKey) {
  const prefix = scoped(rootKey, "");
  for (const request of requests) {
    const path = new URL(request.url).pathname;
    if (!path.startsWith("/api/")) continue;
    assert.ok(path === "/api/checkouts" || path === "/api/daemon/status" || path.startsWith(prefix),
      `browser request bypassed explicit checkout selection: ${request.method} ${path}`);
  }
}
let binary, suiteBinaryDir;

const delay = ms => new Promise(resolveDelay => setTimeout(resolveDelay, ms));
// Functional tests assert correctness, not speed: bounds only detect hangs
// (matches trellis::testing::HANG_BACKSTOP, above serve's replacement wait).
const HANG_BACKSTOP_MS = 120000;
function run(command, args, {cwd = ROOT, env = process.env, timeout = 180000} = {}) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, {cwd, env, stdio:["ignore", "pipe", "pipe"]});
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
    child.on("exit", (code, signal) => {
      if (settled) return;
      settled = true; clearTimeout(timer);
      if (code === 0) resolveRun({stdout, stderr});
      else reject(new Error(`${basename(command)} ${args.join(" ")} failed (${code ?? signal}):\n${stderr.slice(-16000)}`));
    });
  });
}

test.before(async () => {
  const metadata = await run("cargo", ["metadata", "--no-deps", "--format-version", "1"], {timeout:60000});
  const target = JSON.parse(metadata.stdout).target_directory;
  assert.equal(typeof target, "string");
  await run("cargo", ["build", "--locked", "--bin", "trellis"], {timeout:900000});
  const built = join(target, "debug", process.platform === "win32" ? "trellis.exe" : "trellis");
  assert.ok(existsSync(built), `built Trellis binary is unavailable at ${built}`);
  // tools/verify runs Cargo tests and browser acceptance concurrently. Cargo may
  // replace its target executable between these two real-browser scenarios.
  suiteBinaryDir = mkdtempSync(join(tmpdir(), "trellis-saved-binary-"));
  binary = join(suiteBinaryDir, basename(built));
  copyFileSync(built, binary);
  assert.ok(existsSync(binary), `fixed suite binary is unavailable at ${binary}`);
}, {timeout:960000});
test.after(() => { if (suiteBinaryDir) rmSync(suiteBinaryDir, {recursive:true, force:true}); });

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
  return env;
}
function fixture() {
  const temp = mkdtempSync(join(tmpdir(), "trellis-saved-browser-"));
  const paths = {temp, workspace:join(temp,"workspace"), home:join(temp,"home"), xdgCache:join(temp,"xdg-cache"),
    xdgData:join(temp,"xdg-data"), xdgConfig:join(temp,"xdg-config"), cargoHome:join(temp,"cargo-home"), secrets:join(temp,"secrets")};
  for (const path of Object.values(paths).slice(1)) mkdirSync(path, {recursive:true, mode:0o700});
  paths.source = join(paths.workspace, "flow.rs"); paths.tokenFile = join(paths.secrets, "daemon.token"); paths.token = randomBytes(32).toString("hex");
  writeFileSync(paths.source, SOURCE_P); writeFileSync(paths.tokenFile, paths.token, {mode:0o600}); chmodSync(paths.tokenFile,0o600);
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
  const child = spawn(binary, ["serve", "--workspace", paths.workspace, "--bind", "127.0.0.1:0", "--token-file", paths.tokenFile],
    {cwd:ROOT, env:paths.env, stdio:["ignore","ignore","pipe"]});
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
  if (!server?.child || server.child.exitCode !== null) return;
  server.child.kill("SIGTERM");
  const exited = new Promise(resolveExit => server.child.once("exit",resolveExit));
  if (await Promise.race([exited.then(()=>true),delay(3000).then(()=>false)])) return;
  server.child.kill("SIGKILL"); await Promise.race([exited,delay(2000)]);
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
  const symbols=await api(server.base,token,"GET",scoped(server.rootKey,`symbols?q=${encodeURIComponent(name)}`));assert.equal(symbols.status,200,JSON.stringify(symbols.data));
  const item=symbols.data.items.find(value=>value.name===name);assert.ok(item,`missing ${name} symbol`);return item.id;
}
async function connect(page, server, token) {
  const {base,rootKey} = server;
  page.checkoutRootKey = rootKey;
  page.serveLog = () => server.stderr().replaceAll(token,"[redacted]").slice(-4000);
  await page.goto(`${base}/`,{waitUntil:"domcontentloaded"});
  await page.locator("#token").fill(token);
  await page.locator("#connect-form button").click();
  await page.locator("#checkout-select").waitFor({state:"visible",timeout:15000});
  await page.locator("#checkout-select").selectOption(rootKey);
  await page.locator("#workspace").waitFor({state:"visible",timeout:15000});
  await page.waitForFunction(() => document.querySelector("#status")?.textContent?.includes("Revision"),null,{timeout:15000});
}
async function openTools(page) {
  await page.locator("#view-tools").click();
  await page.locator("#tools-panel").waitFor({state:"visible"});
  if(!await page.locator("#secondary-tools").evaluate(node=>node.open)) await page.locator("#secondary-tools > summary").click();
}
// The outcome of the page's next request to `suffix` after `trigger`: its
// response status and body, its failure, or that none came within the hang
// backstop and how many such requests the page issued. A wait never ends in a
// bare timeout that hides a refusal or a request that was never sent (#131).
async function nextPageApi(page, suffix, trigger) {
  const path = browserPath(page,suffix), matches = request => new URL(request.url()).pathname === path;
  const started = Date.now();
  let settle, issued = 0;
  const outcome = new Promise(resolveOutcome => { settle = resolveOutcome; });
  const onRequest = request => { if (matches(request)) issued++; };
  const onResponse = response => {
    if (!matches(response.request())) return;
    settle(response.text().then(body => ({status:response.status(), body:body.slice(0,2000)}),
      error => ({status:response.status(), body:`unreadable: ${error.message}`})));
  };
  const onFailed = request => { if (matches(request)) settle({failure:request.failure()?.errorText ?? "unknown"}); };
  page.on("request",onRequest);page.on("response",onResponse);page.on("requestfailed",onFailed);
  const timer = setTimeout(() => settle({noResponse:true}), HANG_BACKSTOP_MS);
  try { await trigger(); return {suffix, ...await outcome, issued, elapsedMs:Date.now()-started}; }
  finally { clearTimeout(timer); page.off("request",onRequest); page.off("response",onResponse); page.off("requestfailed",onFailed); }
}
// Repeat `trigger` until its request answers 200. Only the daemon's typed
// transient refusals (contention, or no published head yet) are retried, as a
// user would; each is logged. Anything else fails with what was observed and
// the serve log.
async function pageApiAnswered(page, suffix, trigger) {
  for (const deadline = Date.now()+HANG_BACKSTOP_MS;;) {
    const outcome = await nextPageApi(page,suffix,trigger);
    if (outcome.elapsedMs > 5000) console.error(`slow ${suffix}: ${JSON.stringify(outcome).slice(0,500)}`);
    if (outcome.status === 200) return outcome;
    let code = null; try { code = JSON.parse(outcome.body).error?.code ?? null; } catch {}
    if (outcome.status === 503 && ["storage_busy","index_not_ready"].includes(code) && Date.now() < deadline) {
      console.error(`retrying ${suffix} after a typed refusal: ${JSON.stringify(outcome)}`);
      await delay(25); continue;
    }
    assert.fail(`${suffix} was not answered: ${JSON.stringify(outcome)}; serve log: ${page.serveLog?.() ?? "unavailable"}`);
  }
}
async function selectSymbol(page, name) {
  await page.locator("#search").fill(name);
  await pageApiAnswered(page,"symbols",() => page.locator("#search-form").evaluate(form=>form.requestSubmit()));
  const pick=page.locator("#symbols button.symbol").filter({hasText:name}).first();await pick.waitFor({state:"visible"});
  await pageApiAnswered(page,"query",() => pick.click());
}
const row = (page, kind, id) => page.locator(`li[data-saved-kind="${kind}"][data-saved-id="${id}"]`);
// Bounded snapshot of a saved row and page notice for failure messages (#131).
async function savedDiagnostics(page, kind, id) {
  const quick = {timeout:1000};
  const target = row(page,kind,id), load = target.getByRole("button",{name:"Load",exact:true});
  const read = async probe => { try { return await probe(); } catch (error) { return `unavailable: ${error.message.split("\n")[0]}`; } };
  return JSON.stringify({
    rowVisible:await read(() => target.isVisible()),
    rowText:await read(async () => (await target.textContent(quick))?.slice(0,300)),
    loadEnabled:await read(() => load.isEnabled(quick)),
    loadReason:await read(() => load.getAttribute("title",quick)),
    notice:await read(async () => (await page.locator("#notice").textContent(quick))?.slice(0,300)),
    pageRevision:await read(() => page.evaluate(() => status?.revision ?? null)),
  });
}
function readPayload(dbPath, kind, id) {
  assert.ok(["view","note"].includes(kind));
  const db=openDb(dbPath,{readOnly:true});
  try {
    const version=db.prepare("PRAGMA user_version").get().user_version;assert.equal(version,1);
    const table=kind==="view"?"views":"annotations";
    const value=db.prepare(`SELECT payload FROM ${table} WHERE id=?`).get(id);assert.ok(value,`missing ${kind} ${id}`);return value.payload;
  } finally { db.close(); }
}
function rawJsonProperty(payload, key) {
  const marker=`"${key}"`;let at=payload.indexOf(marker);if(at<0)return null;
  at=payload.indexOf(":",at+marker.length)+1;while(/\s/.test(payload[at]))at++;
  const start=at;let depth=0,string=false,escape=false;
  for(;at<payload.length;at++){
    const char=payload[at];
    if(string){if(escape)escape=false;else if(char==="\\")escape=true;else if(char==='"')string=false;continue;}
    if(char==='"'){string=true;continue;} if(char==="{"||char==="[")depth++;else if(char==="}"||char==="]"){if(depth===0)break;depth--;}
    if(depth===0&&(char===","||char==="}"))break;
  }
  return payload.slice(start,at).trim();
}
// The user-level daemon outlives serve and may still be writing the store when
// the test reads or writes it directly: wait on a lock (bounded by the hang
// backstop) instead of failing with SQLITE_BUSY.
function openDb(dbPath, options={}) {
  const db=new DatabaseSync(dbPath,options);
  db.exec(`PRAGMA busy_timeout=${HANG_BACKSTOP_MS}`);
  return db;
}
function insertLegacy(dbPath, kind, id, seed) {
  const db=openDb(dbPath);
  try {
    if(kind==="view") db.prepare("INSERT INTO views(id,payload) VALUES(?,?)").run(id,JSON.stringify({id,title:"Legacy view",query:{seed},pins:{},hidden:[]}));
    else db.prepare("INSERT INTO annotations(id,node_id,payload) VALUES(?,?,?)").run(id,seed,JSON.stringify({id,nodeId:seed,body:"Legacy body"}));
  } finally { db.close(); }
}

for (const kind of ["view","note"]) test(`real browser ${kind} save, edit, stale replay, orphan, and legacy safety`, {timeout:180000}, async t => {
  const paths=fixture();let server,browser,context,page;const pageErrors=[];
  t.after(async()=>{try{await context?.close();}catch{}try{await browser?.close();}catch{}await stopDaemon(server);rmSync(paths.temp,{recursive:true,force:true,maxRetries:10,retryDelay:100});});
  const indexed=await run(binary,["index","--workspace",paths.workspace],{env:paths.env,timeout:30000});
  const initialPin=JSON.parse(indexed.stdout).status.revision;
  // Serve registers in place on the indexed checkout (Decision 0009 §6): no
  // new startup reconciliation, so the indexed pin stays the head.
  server=await startDaemon(paths,initialPin.indexRevision);
  const servingStatus=await api(server.base,paths.token,"GET",scoped(server.rootKey,"status"));
  assert.equal(servingStatus.status,200,JSON.stringify(servingStatus.data));
  const originalRootKey=server.rootKey;
  const P=servingStatus.data.revision;
  assert.equal(P.indexGeneration,initialPin.indexGeneration);
  assert.equal(P.indexRevision,initialPin.indexRevision);
  let wrongToken; do { wrongToken = randomBytes(32).toString("hex"); } while (wrongToken === paths.token);
  const denied = await fetch(`${server.base}${scoped(server.rootKey,"views")}`, {signal:AbortSignal.timeout(8000),
    headers:{Origin:server.base,Authorization:`Bearer ${wrongToken}`}});
  assert.equal(denied.status,401,"a wrong runtime bearer must not read saved items");
  assert.deepEqual(findNamed(paths.temp,"workspace.db"),[],"denied saved reads must not create workspace.db");
  browser=await chromium.launch({headless:true});context=await browser.newContext();page=await context.newPage();
  page.on("pageerror",error=>pageErrors.push(error));
  const requests=[];page.on("request",request=>requests.push({url:request.url(),method:request.method(),body:request.postDataJSON?.()}));
  // The page's own in-flight API requests: one still open when head moves can
  // observe the new head and re-render saved rows away from the stale pin (#131).
  let pageApiInFlight=0;const isApi=request=>new URL(request.url()).pathname.startsWith("/api/");
  page.on("request",request=>{if(isApi(request))pageApiInFlight++;});
  for(const done of ["requestfinished","requestfailed"])page.on(done,request=>{if(isApi(request))pageApiInFlight--;});
  await connect(page,server,paths.token);
  const initialLists=requests.filter(request=>[browserPath(page,"views"),browserPath(page,"annotations")].includes(new URL(request.url).pathname));
  assert.equal(initialLists.length,2);for(const request of initialLists)assertPinnedUrl(request.url,P);
  assert.deepEqual(findNamed(paths.temp,"workspace.db"),[],"saved reads must not create workspace.db");
  await openTools(page);await selectSymbol(page,"seed");const seed=await seedId(server,paths.token,"seed");

  const putPath=browserPath(page,kind==="view"?"views/":"annotations/");
  if(kind==="view") await page.locator("#view-title").fill("Saved view");
  else {await page.locator("#note-title").fill("Saved note");await page.locator("#note").fill("First body");}
  const putRequestPromise=page.waitForRequest(request=>request.method()==="PUT"&&new URL(request.url()).pathname.startsWith(putPath));
  const putResponsePromise=page.waitForResponse(response=>response.request().method()==="PUT"&&new URL(response.url()).pathname.startsWith(putPath));
  await page.locator(kind==="view"?"#save-view":"#save-note").click();
  const putRequest=await putRequestPromise,putResponse=await putResponsePromise,putBody=putRequest.postDataJSON(),putState=await putResponse.json();
  assert.equal(putResponse.status(),200);assertPinnedUrl(putRequest.url(),P);
  const id=putBody.id;assert.equal(kind==="view"?putBody.query.seed:putBody.nodeId,seed);
  assert.deepEqual(Object.keys(putBody).sort(),kind==="view"?["hidden","id","pins","query","title"]:["body","id","nodeId","title"]);
  assert.equal(Object.hasOwn(putBody,"anchor"),false);assert.equal(putState.indexGeneration,P.indexGeneration);assert.equal(putState.indexRevision,P.indexRevision);
  assert.equal(putState.attachment.availability,"ready");assert.equal(putState.attachment.result.status,"attached");assert.equal(putState.attachment.result.targetId,seed);
  const savedRow=row(page,kind,id);await savedRow.waitFor({state:"visible"});assert.equal(await savedRow.getByRole("button",{name:"Load",exact:true}).isEnabled(),true);

  let baseline=requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length;
  const loadRequestPromise=page.waitForRequest(request=>request.method()==="POST"&&new URL(request.url()).pathname===browserPath(page,"query")&&new URL(request.url()).searchParams.has("indexGeneration"));
  await savedRow.getByRole("button",{name:"Load",exact:true}).click();const loadRequest=await loadRequestPromise,loadResponse=await loadRequest.response();
  assert.ok(loadResponse);assert.equal(loadResponse.status(),200);assertPinnedUrl(loadRequest.url(),P);assert.equal(loadRequest.postDataJSON().seed,seed);
  assert.deepEqual((await loadResponse.json()).revision,P);await delay(250);
  assert.equal(requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length,baseline+1);

  await savedRow.getByRole("button",{name:kind==="view"?"Edit title":"Edit",exact:true}).click();
  await selectSymbol(page,"other");
  if(kind==="view") await page.locator("#view-title").fill("Edited view");
  else {await page.locator("#note-title").fill("Edited note");await page.locator("#note").fill("Edited body");}
  const editRequestPromise=page.waitForRequest(request=>request.method()==="PUT"&&new URL(request.url()).pathname===`${putPath}${id}`);
  await page.locator(kind==="view"?"#save-view":"#save-note").click();const editRequest=await editRequestPromise,editBody=editRequest.postDataJSON();
  const editResponse=await editRequest.response();assert.ok(editResponse);assert.equal(editResponse.status(),200);
  assertPinnedUrl(editRequest.url(),P);assert.equal(kind==="view"?editBody.query.seed:editBody.nodeId,seed);assert.equal(Object.hasOwn(editBody,"anchor"),false);
  await page.reload({waitUntil:"domcontentloaded"});await connect(page,server,paths.token);await openTools(page);if(kind==="note")await selectSymbol(page,"seed");
  await row(page,kind,id).waitFor({state:"visible"});assert.match(await row(page,kind,id).textContent(),kind==="view"?/Edited view/:/Edited note[\s\S]*Edited body/);

  const dbs=findNamed(paths.temp,"workspace.db");assert.equal(dbs.length,1,JSON.stringify(dbs));const dbPath=dbs[0];
  const anchorBefore=rawJsonProperty(readPayload(dbPath,kind,id),"anchor");assert.ok(anchorBefore?.startsWith("{"),anchorBefore);
  const staleRow=row(page,kind,id);assert.equal(await staleRow.getByRole("button",{name:"Load",exact:true}).isEnabled(),true);
  // Reach and hold the stale-P page state: let every page request finish
  // before head moves. The page starts no requests of its own without a job.
  for(const idleBy=Date.now()+30000,quiet={since:Date.now()};;await delay(25)){
    if(pageApiInFlight>0)quiet.since=Date.now();else if(Date.now()-quiet.since>=300)break;
    if(Date.now()>idleBy)throw new Error(`page never went idle before the edit: ${await savedDiagnostics(page,kind,id)}`);
  }
  writeFileSync(paths.source,SOURCE_Q);const Q=await indexThroughApi(server,paths.token,P,basename(paths.source),SOURCE_Q);
  assert.equal(await seedId(server,paths.token,"seed"),seed,"header edit must retain the original ordinal declaration ID");
  // The page must still be at P with nothing in flight: that is the stale state
  // under test. Anything else is reported, not waited out.
  const pageRevision=await page.evaluate(()=>status?.revision??null);
  assert.deepEqual(pageRevision,P,`page left the stale pin during indexing: ${await savedDiagnostics(page,kind,id)}`);
  assert.equal(pageApiInFlight,0,`page request in flight after indexing: ${await savedDiagnostics(page,kind,id)}`);
  assert.equal(await staleRow.getByRole("button",{name:"Load",exact:true}).isEnabled(),true,`stale Load disabled at P: ${await savedDiagnostics(page,kind,id)}`);
  baseline=requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length;
  const staleRequestPromise=page.waitForRequest(request=>request.method()==="POST"&&new URL(request.url()).pathname===browserPath(page,"query")&&new URL(request.url()).searchParams.has("indexGeneration"))
    .catch(async error=>{throw new Error(`${error.message}; after Load: ${await savedDiagnostics(page,kind,id)}`);});
  const staleResponsePromise=page.waitForResponse(response=>response.request().method()==="POST"&&new URL(response.url()).pathname===browserPath(page,"query")&&new URL(response.url()).searchParams.has("indexGeneration"));
  await staleRow.getByRole("button",{name:"Load",exact:true}).click();const staleRequest=await staleRequestPromise,staleResponse=await staleResponsePromise;
  assertPinnedUrl(staleRequest.url(),P);assert.equal(staleRequest.postDataJSON().seed,seed);assert.equal(staleResponse.status(),200);
  const staleData=await staleResponse.json();assert.deepEqual(staleData.revision,P,`pinned replay ${new URL(staleRequest.url()).pathname}${new URL(staleRequest.url()).search} must return saved revision P=${JSON.stringify(P)}, not Q=${JSON.stringify(Q)}`);assert.notDeepEqual(staleData.revision,Q);
  assert.ok(staleData.calls.some(call=>call.calleeText==="old_step"),JSON.stringify(staleData));
  assert.doesNotMatch(JSON.stringify(staleData),/new_step/);await delay(800);
  assert.equal(requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length,baseline+1,"stale replay must not retry");
  assert.doesNotMatch(await page.locator("#calls").textContent(),/new_step/);

  await page.reload({waitUntil:"domcontentloaded"});await connect(page,server,paths.token);await openTools(page);
  const queryCountAfterReconnect=requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length;assert.equal(queryCountAfterReconnect,baseline+1);
  const orphanRow=row(page,kind,id);await orphanRow.waitFor({state:"visible"});assert.match(await orphanRow.textContent(),/header changed/i);
  const disabledLoad=orphanRow.getByRole("button",{name:"Load",exact:true});assert.equal(await disabledLoad.isDisabled(),true);
  const box=await disabledLoad.boundingBox();assert.ok(box);await page.mouse.click(box.x+box.width/2,box.y+box.height/2);await delay(250);
  assert.equal(requests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length,queryCountAfterReconnect);
  const stateRoute=kind==="view"?`views/${id}${pinSuffix(Q)}`:`annotations${pinSuffix(Q)}`;
  const direct=await api(server.base,paths.token,"GET",scoped(server.rootKey,stateRoute));assert.equal(direct.status,200,JSON.stringify(direct.data));
  const directState=kind==="view"?direct.data:direct.data.find(item=>item.annotation.id===id);assert.ok(directState);
  assert.equal(directState.indexGeneration,Q.indexGeneration);assert.equal(directState.indexRevision,Q.indexRevision);assert.equal(directState.attachment.result.status,"orphaned");
  assert.equal(directState.attachment.result.reason,"headerMismatch");assert.equal(directState.attachment.result.targetId,null);

  await selectSymbol(page,"other");await orphanRow.getByRole("button",{name:kind==="view"?"Edit title":"Edit",exact:true}).click();
  if(kind==="view")await page.locator("#view-title").fill("Orphan edited view");
  else {await page.locator("#note-title").fill("Orphan edited note");await page.locator("#note").fill("Orphan edited body");}
  const orphanEditPromise=page.waitForRequest(request=>request.method()==="PUT"&&new URL(request.url()).pathname===`${putPath}${id}`);
  await page.locator(kind==="view"?"#save-view":"#save-note").click();const orphanEdit=await orphanEditPromise;
  const orphanEditResponse=await orphanEdit.response();assert.ok(orphanEditResponse);assert.equal(orphanEditResponse.status(),200);
  assertPinnedUrl(orphanEdit.url(),Q);assert.equal(kind==="view"?orphanEdit.postDataJSON().query.seed:orphanEdit.postDataJSON().nodeId,seed);
  const anchorAfter=rawJsonProperty(readPayload(dbPath,kind,id),"anchor");assert.equal(anchorAfter,anchorBefore,"title/body/orphan edits must preserve raw anchor bytes");

  await context.close();context=null;await stopDaemon(server);server=null;
  const legacyId=`legacy-${kind}`;insertLegacy(dbPath,kind,legacyId,seed);
  server=await startDaemon(paths,Q.indexRevision);
  const reopenedStatus=await api(server.base,paths.token,"GET",scoped(server.rootKey,"status"));
  assert.equal(reopenedStatus.status,200,JSON.stringify(reopenedStatus.data));
  const R=reopenedStatus.data.revision;
  assert.equal(server.rootKey,originalRootKey,"reopened checkout must retain identity");
  assert.equal(R.indexGeneration,Q.indexGeneration);
  assert.equal(R.indexRevision,Q.indexRevision,"serve restart must retain the latest indexed revision");
  context=await browser.newContext();page=await context.newPage();page.on("pageerror",error=>pageErrors.push(error));
  const legacyRequests=[];page.on("request",request=>legacyRequests.push({url:request.url(),method:request.method(),body:request.postDataJSON?.()}));
  await connect(page,server,paths.token);await openTools(page);
  const persistedOrphan=row(page,kind,id);await persistedOrphan.waitFor({state:"visible"});
  assert.match(await persistedOrphan.textContent(),kind==="view"?/Orphan edited view/:/Orphan edited note[\s\S]*Orphan edited body/);
  const legacyRow=row(page,kind,legacyId);await legacyRow.waitFor({state:"visible"});
  assert.match(await legacyRow.textContent(),/no durable anchor/i);const legacyLoad=legacyRow.getByRole("button",{name:"Load",exact:true});assert.equal(await legacyLoad.isDisabled(),true);
  await legacyRow.getByRole("button",{name:kind==="view"?"Edit title":"Edit",exact:true}).click();
  if(kind==="view")await page.locator("#view-title").fill("Legacy view edited");else {await page.locator("#note-title").fill("Legacy note");await page.locator("#note").fill("Legacy body edited");}
  const legacyPutPromise=page.waitForRequest(request=>request.method()==="PUT"&&new URL(request.url()).pathname===`${putPath}${legacyId}`);
  await page.locator(kind==="view"?"#save-view":"#save-note").click();const legacyPut=await legacyPutPromise;
  const legacyPutResponse=await legacyPut.response();assert.ok(legacyPutResponse);assert.equal(legacyPutResponse.status(),200);assertPinnedUrl(legacyPut.url(),R);
  assert.equal(kind==="view"?legacyPut.postDataJSON().query.seed:legacyPut.postDataJSON().nodeId,seed);assert.equal(Object.hasOwn(JSON.parse(readPayload(dbPath,kind,legacyId)),"anchor"),false);
  // The list is replaced after a later loadSaved(); wait for the re-rendered row carrying the edited title, never the pre-PUT row.
  const legacyRowAfter=row(page,kind,legacyId).filter({hasText:kind==="view"?"Legacy view edited":"Legacy note"});await legacyRowAfter.waitFor({state:"visible"});
  const legacyLoadAfter=legacyRowAfter.getByRole("button",{name:"Load",exact:true});await legacyLoadAfter.waitFor({state:"visible"});const legacyBox=await legacyLoadAfter.boundingBox();assert.ok(legacyBox);
  const legacyQueryBaseline=legacyRequests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length;await page.mouse.click(legacyBox.x+legacyBox.width/2,legacyBox.y+legacyBox.height/2);await delay(250);
  assert.equal(legacyRequests.filter(request=>new URL(request.url).pathname===browserPath(page,"query")).length,legacyQueryBaseline);
  const deleteResponse=page.waitForResponse(response=>response.request().method()==="DELETE"&&new URL(response.url()).pathname===`${putPath}${legacyId}`);
  await row(page,kind,legacyId).getByRole("button",{name:"Delete",exact:true}).click();assert.equal((await deleteResponse).status(),204);
  assert.equal(rawJsonProperty(readPayload(dbPath,kind,id),"anchor"),anchorBefore,"legacy deletion must not change the anchored item");
  assertSelectedBrowserRequests(requests,originalRootKey);
  assertSelectedBrowserRequests(legacyRequests,server.rootKey);
  assert.deepEqual(pageErrors.map(error=>error.message),[]);
});
