"use strict";
// #906: after a daemon stall the open tab recovers by itself, a reload keeps the saved sign-in,
// and an agent show that the stall stopped is shown once the daemon answers again.
// The stall is simulated in the browser: while it lasts, the selected checkout's API requests
// answer 503 or fail as a timed-out connection. Everything else is the real daemon.
const test = require("node:test");
const assert = require("node:assert/strict");
const {spawn} = require("node:child_process");
const {rmSync} = require("node:fs");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const KEY = "trellis.daemonToken.v1";
const source = "function inspect(input) { return helper(input); }\nfunction helper(value) { return value + 1; }\n";
// A hang backstop only (functional tests carry no speed bound): recovery backs off from 2 s.
const RECOVERED_MS = 60000;
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published(callback) {
  const paths = H.fixture(source, "flow.js");
  let server, browser, context, page;
  try {
    // `trellis mcp` attaches a Git checkout.
    await H.run("git", ["init", "-q", paths.workspace], {env:paths.env});
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    server = await H.startDaemon(paths, indexed.json().status.revision.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    page = await context.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await callback({page, paths, server, pin:indexed.json().status.revision});
    assert.deepEqual(errors, []);
  } catch (error) {
    // Say what the page showed when a wait gave up.
    const shown = await page?.evaluate(() => Object.fromEntries(["error", "notice", "sequence-state", "source-path", "workspace-index-label", "agent-shown-label"]
      .map(id => [id, `${document.getElementById(id)?.hidden ? "(hidden) " : ""}${document.getElementById(id)?.textContent || ""}`]))).catch(() => null);
    throw new Error(`${error.message}\npage: ${JSON.stringify(shown)}`, {cause:error});
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
// While `stall.on`, every request to the selected checkout's API answers an untyped 503, an
// HTML 503 or an empty 502, or fails as a timed-out connection, in turn. `stall.only` narrows it to one route.
async function stallable(page, server) {
  const stall = {on:false, only:null, failed:[]};
  await page.route(url => url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`), async route => {
    const path = new URL(route.request().url()).pathname.slice(`/api/checkouts/${server.rootKey}/`.length);
    if (!stall.on || (stall.only && path !== stall.only)) return route.fallback();
    stall.failed.push(path);
    // What a stalled daemon or a proxy in front of it sends: untyped JSON, HTML, nothing, or no answer.
    switch (stall.failed.length % 4) {
      case 1: return route.fulfill({status:503, contentType:"application/json", body:"{}"});
      case 2: return route.fulfill({status:503, contentType:"text/html", body:"<html><body>Service Unavailable</body></html>"});
      case 3: return route.fulfill({status:502, body:""});
      default: return route.abort("timedout");
    }
  });
  return stall;
}
async function openMethod(page, name) {
  const method = page.locator(".method-pick").filter({hasText:name}).first();
  if (!await method.isVisible()) await page.locator(".file-pick").filter({hasText:"flow.js"}).first().click();
  await method.click();
}
const sequenceShown = name => page => page.waitForFunction(name => {
  const text = document.querySelector("#sequence-state")?.textContent || "";
  return text.startsWith(`${name} · flow.js · revision `);
}, name, {timeout:RECOVERED_MS});

test("after a stall the tab reloads its failed view and clears Catching up from fresh status, without a reload", {timeout:240000}, async () => published(async ({page, paths, server}) => {
  await H.connect(page, server, paths.token);
  await openMethod(page, "inspect");
  await sequenceShown("inspect")(page);
  // An automatic re-run: status says catching up. The head digest says the same, unconditionally.
  let catchingUp = true;
  await page.route(url => url.pathname === `/api/checkouts/${server.rootKey}/status` || url.pathname === `/api/checkouts/${server.rootKey}/status/head`, async route => {
    if (!catchingUp) return route.fallback();
    const headers = {...route.request().headers()}; delete headers["if-none-match"];
    const response = await route.fetch({headers});
    const body = await response.json();
    return route.fulfill({status:200, contentType:"application/json", body:JSON.stringify({...body, catchingUp:true})});
  });
  const stall = await stallable(page, server);
  await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
  await page.waitForFunction(() => !document.querySelector("#catching-up").hidden);
  // The daemon stalls; a method chosen now cannot load.
  stall.on = true;
  await openMethod(page, "helper");
  await page.waitForFunction(() => /Request (failed|timed out)|Failed to fetch/.test(document.querySelector("#sequence-state")?.textContent || ""));
  assert.ok(stall.failed.includes("sequence"), stall.failed.join());
  // The daemon answers again and the re-run has finished. Nobody touches the page.
  catchingUp = false; stall.on = false;
  await sequenceShown("helper")(page);
  await page.waitForFunction(() => document.querySelector("#catching-up").hidden, null, {timeout:RECOVERED_MS});
  await page.waitForFunction(() => !/Catching up/.test(document.querySelector("#workspace-index-label")?.textContent || ""), null, {timeout:RECOVERED_MS});
  assert.equal(await page.locator("#error").evaluate(node => node.hidden), true, "the stall's error is cleared by the successful retry");
}));

test("a reload after a stall keeps the remembered sign-in", {timeout:240000}, async () => published(async ({page, paths, server}) => {
  await page.goto(`${server.base}/`, {waitUntil:"domcontentloaded"});
  await page.locator("#token").fill(paths.token);
  await page.locator("#remember-token").check();
  await page.locator("#connect-form button[type=submit]").click();
  await page.locator("#workspace").waitFor({state:"visible"});
  await page.waitForFunction(() => document.querySelector("#status")?.textContent?.includes("Revision"));
  await openMethod(page, "inspect");
  await sequenceShown("inspect")(page);
  assert.equal(await page.evaluate(key => localStorage.getItem(key), KEY), paths.token);
  // During the stall: deadlines, 503s, and a typed refusal to read one source path, which is
  // not an authentication rejection.
  const stall = await stallable(page, server);
  await page.route(url => url.pathname === `/api/checkouts/${server.rootKey}/source`, route => stall.on
    ? route.fulfill({status:403, contentType:"application/json", body:JSON.stringify({error:{code:"source_forbidden", message:"Source access is not allowed"}})})
    : route.fallback());
  stall.on = true;
  await page.locator("#method-source").click();
  await page.waitForFunction(() => /Source access is not allowed/.test(document.querySelector("#error")?.textContent || ""));
  await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
  await page.waitForFunction(() => /Request (failed|timed out)|Failed to fetch/.test(document.querySelector("#error")?.textContent || ""));
  assert.equal(await page.locator("#workspace").isVisible(), true, "a stall keeps the page signed in");
  stall.on = false;
  await page.reload({waitUntil:"domcontentloaded"});
  await page.locator("#connect-form").waitFor({state:"visible"});
  assert.ok(await page.locator("#token").evaluate((input, secret) => input.value === secret, paths.token), "the saved token is restored");
  assert.equal(await page.locator("#remember-token").isChecked(), true);
  await page.locator("#connect-form button[type=submit]").click();
  await page.locator("#workspace").waitFor({state:"visible"});
  await page.waitForFunction(() => document.querySelector("#status")?.textContent?.includes("Revision"));
  assert.equal(await page.evaluate(key => localStorage.getItem(key), KEY), paths.token);
}));

// A legacy MCP session on the real daemon, as an agent's client uses it.
function mcpSession(paths) {
  const child = spawn(H.binary, ["mcp", "--workspace", paths.workspace], {env:paths.env, stdio:["pipe", "pipe", "pipe"]});
  let buffer = "", next = 0, stderr = "";
  child.stderr.on("data", chunk => { stderr = (stderr + chunk).slice(-4000); });
  const waiting = new Map();
  child.stdout.on("data", chunk => {
    buffer += chunk;
    for (let at = buffer.indexOf("\n"); at >= 0; at = buffer.indexOf("\n")) {
      const reply = JSON.parse(buffer.slice(0, at)); buffer = buffer.slice(at + 1);
      waiting.get(reply.id)?.(reply); waiting.delete(reply.id);
    }
  });
  const send = frame => child.stdin.write(`${JSON.stringify(frame)}\n`);
  const ask = (method, params) => new Promise((resolve, reject) => {
    const id = ++next, timer = setTimeout(() => reject(new Error(`${method} hung: exit=${child.exitCode} stderr=${stderr.replaceAll(paths.token, "[redacted]")}`)), 60000);
    waiting.set(id, reply => { clearTimeout(timer); resolve(reply); });
    send({jsonrpc:"2.0", id, method, params});
  });
  return {
    async open() {
      const reply = await ask("initialize", {protocolVersion:"2025-11-25", capabilities:{}, clientInfo:{name:"browser-test", version:"1"}});
      assert.ok(reply.result, JSON.stringify(reply));
      send({jsonrpc:"2.0", method:"notifications/initialized"});
    },
    async show(args) {
      const reply = await ask("tools/call", {name:"trellis_show", arguments:{schemaVersion:1, ...args}});
      return reply.result?.structuredContent;
    },
    close() { child.kill("SIGTERM"); },
  };
}

test("an agent show that a stall stopped is shown once the daemon answers again", {timeout:240000}, async () => published(async ({page, paths, server}) => {
  await H.connect(page, server, paths.token);
  await openMethod(page, "inspect");
  await sequenceShown("inspect")(page);
  const helper = await H.seedId(server, paths.token, "helper");
  // The tab's status poll answers, but the show's sequence read stalls: the frozen tab of #906.
  const stall = await stallable(page, server);
  stall.only = "sequence"; stall.on = true;
  const mcp = mcpSession(paths);
  try {
    await mcp.open();
    const answer = await mcp.show({view:"sequence", symbolId:helper});
    assert.equal(answer?.data?.outcome, "shown", JSON.stringify(answer));
    await page.waitForFunction(() => /Request (failed|timed out)|Failed to fetch/.test(document.querySelector("#error")?.textContent || ""), null, {timeout:RECOVERED_MS});
    assert.ok(stall.failed.includes("sequence"), stall.failed.join());
    assert.equal(await page.locator("#agent-shown").evaluate(node => node.hidden), true, "a stopped show is not reported as shown");
    stall.on = false;
    await page.waitForFunction(() => !document.querySelector("#agent-shown").hidden &&
      /Shown by agent · Sequence · helper/.test(document.querySelector("#agent-shown-label").textContent), null, {timeout:RECOVERED_MS});
    await sequenceShown("helper")(page);
  } finally { mcp.close(); }
}));
