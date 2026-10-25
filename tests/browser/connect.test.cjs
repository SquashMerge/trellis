"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {rmSync} = require("node:fs");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const KEY = "trellis.daemonToken.v1";
const source = "function inspect(input) { return input; }\n";
const wrongToken = "synthetic-wrong-token";
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function disconnected(page) {
  await page.locator("#connect-form").waitFor({state:"visible"});
  assert.equal(await page.locator("#token").getAttribute("type"), "password");
  assert.equal(await page.locator("#workspace").isVisible(), false);
}
async function connectedWithoutTokenInDom(page, server, token) {
  await page.waitForFunction(host => document.querySelector("#notice")?.textContent.includes(`Connected to the daemon on ${host}.`), new URL(server.base).host);
  assert.equal(await page.locator("#connect-form").isVisible(), false);
  assert.ok(await page.locator("#token").evaluate(input => input.value === ""), "connected password input is cleared");
  assert.ok(await page.evaluate(secret => !document.documentElement.outerHTML.includes(secret) && !document.body.textContent.includes(secret), token), "connected DOM does not contain the credential");
  assert.equal(new URL(page.url()).search, "");
  assert.equal(new URL(page.url()).hash, "");
}
async function hasSavedToken(page, token) {
  return page.evaluate(({key, secret}) => localStorage.getItem(key) === secret, {key:KEY, secret:token});
}
async function hasNoSavedToken(page) {
  return page.evaluate(key => localStorage.getItem(key) === null, KEY);
}

test("real Connect preserves explicit token opt-in, file loading, restore, typed rejection and Disconnect", {timeout:240000}, async () => {
  const paths = H.fixture(source, "flow.js");
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    server = await H.startDaemon(paths, JSON.parse(indexed.stdout).status.revision.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440,height:900}});
    const page = await context.newPage();
    const apiRequests = [], pageErrors = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    page.on("request", request => {
      const url = new URL(request.url());
      assert.ok(!request.url().includes(paths.token) && !request.url().includes(wrongToken), "credentials are never placed in a URL");
      assert.ok(!(request.postData() || "").includes(paths.token), "credential is not sent in a request body");
      if (url.pathname.startsWith("/api/")) apiRequests.push({
        path:url.pathname, method:request.method(),
        validBearer:request.headers().authorization === `Bearer ${paths.token}`,
      });
    });
    await page.goto(`${server.base}/`, {waitUntil:"networkidle"});
    await disconnected(page);
    assert.ok(await page.locator("#token").evaluate(input => input.value === ""), "fresh browser has no token");
    assert.equal(await page.locator("#remember-token").isChecked(), false);
    assert.deepEqual(apiRequests, [], "loading Connect does not contact authenticated APIs");
    assert.equal(await page.getByRole("heading", {name:"Connect to Trellis", exact:true}).isVisible(), true);
    assert.equal(await page.getByLabel("Daemon token", {exact:true}).isVisible(), true);
    assert.equal(await page.getByRole("button", {name:"Connect", exact:true}).isVisible(), true);
    assert.equal(await page.getByRole("button", {name:"Load token file instead", exact:true}).isVisible(), true);
    assert.equal(await page.locator("#token-file").isVisible(), false, "native file control stays hidden behind its labelled action");
    const desktopLogo = await page.locator(".connection-logo").boundingBox();
    const desktopForm = await page.locator("#connect-form").boundingBox();
    assert.ok(desktopLogo && desktopForm && desktopLogo.x + desktopLogo.width < desktopForm.x, "desktop logo and form are side by side");
    for (const width of [390, 320]) {
      await page.setViewportSize({width, height:844});
      const narrowLogo = await page.locator(".connection-logo").boundingBox();
      const narrowForm = await page.locator("#connect-form").boundingBox();
      assert.ok(narrowLogo && narrowForm && narrowLogo.y + narrowLogo.height <= narrowForm.y, `${width}px layout stacks logo above form`);
      for (const selector of ["#connect-form", "#token", "#load-token-file", "#remember-token", "#connect-form button[type=submit]"]) {
        const bounds = await page.locator(selector).boundingBox();
        assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= width, `${selector} fits ${width}px viewport`);
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px Connect has no horizontal overflow`);
    }
    await page.setViewportSize({width:1440, height:900});

    // The visible file action opens Chromium's real chooser from the keyboard.
    await page.locator("#token").focus();
    await page.keyboard.press("Tab");
    assert.equal(await page.locator("#load-token-file").evaluate(node => document.activeElement === node && node.matches(":focus-visible") && getComputedStyle(node).outlineStyle !== "none" && parseFloat(getComputedStyle(node).outlineWidth) > 0), true, "file action has visible keyboard focus");
    const choosing = page.waitForEvent("filechooser");
    await page.keyboard.press("Enter");
    await (await choosing).setFiles(paths.tokenFile);
    await page.waitForFunction(() => document.querySelector("#notice")?.textContent.includes("Token file loaded locally"));
    assert.ok(await page.locator("#token").evaluate((input, secret) => input.value === secret, paths.token), "local file fills the password field");
    assert.ok(await page.locator("#token-file").evaluate(input => input.value === ""), "file selection resets after loading");
    assert.equal(await page.locator("#remember-token").isChecked(), false);
    assert.equal(await hasNoSavedToken(page), true);
    assert.deepEqual(apiRequests, [], "file selection does not connect or persist");

    // Continue in form order and submit without a pointer.
    await page.locator("#load-token-file").focus();
    await page.keyboard.press("Tab");
    assert.equal(await page.locator("#remember-token").evaluate(node => document.activeElement === node), true);
    await page.keyboard.press("Space");
    await page.keyboard.press("Tab");
    assert.equal(await page.locator("#connect-form button[type=submit]").evaluate(node => document.activeElement === node && node.matches(":focus-visible")), true);
    await page.keyboard.press("Enter");
    await connectedWithoutTokenInDom(page, server, paths.token);
    assert.equal(await hasSavedToken(page, paths.token), true, "successful explicit opt-in saves the token");
    assert.deepEqual(apiRequests.map(request => request.path).sort(), ["/api/checkouts", "/api/daemon/status"]);
    assert.ok(apiRequests.every(request => request.method === "GET" && request.validBearer), "handshake uses only read-only authenticated requests");
    assert.equal(await page.locator("#checkout-select").inputValue(), "", "a single checkout is not auto-selected");
    assert.equal(await page.locator("#workspace").isVisible(), false);

    // The same origin restores convenience, not an authenticated session.
    apiRequests.length = 0;
    await page.reload({waitUntil:"networkidle"});
    await disconnected(page);
    assert.ok(await page.locator("#token").evaluate((input, secret) => input.value === secret, paths.token), "reload restores the remembered value");
    assert.equal(await page.locator("#remember-token").isChecked(), true);
    assert.deepEqual(apiRequests, [], "remembered token does not auto-connect on reload");
    await page.locator("#remember-token").uncheck();
    assert.equal(await hasNoSavedToken(page), true, "unchecking Remember forgets immediately");
    assert.deepEqual(apiRequests, []);
    await page.locator("#remember-token").check();
    await page.locator("#connect-form button[type=submit]").click();
    await connectedWithoutTokenInDom(page, server, paths.token);
    assert.equal(await hasSavedToken(page, paths.token), true);
    assert.ok(apiRequests.every(request => ["/api/checkouts", "/api/daemon/status"].includes(request.path)), "reconnection does not choose a checkout");
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.locator("#workspace").waitFor({state:"visible"});
    await page.waitForFunction(() => document.querySelector("#status")?.textContent.includes("Revision"));
    assert.equal(await page.locator("#error").isVisible(), false);
    assert.ok(apiRequests.some(request => request.path === `/api/checkouts/${server.rootKey}/status`), "explicit selection loads the selected checkout");
    assert.ok(apiRequests.every(request => request.method === "GET" && request.validBearer), "Connect and checkout selection do not index or call providers");
    await page.locator("#logout").click();
    await disconnected(page);
    assert.equal(await hasNoSavedToken(page), true);
    assert.equal(await page.locator("#remember-token").isChecked(), false);
    assert.ok(await page.locator("#token").evaluate(input => input.value === ""), "Disconnect clears password input");
    assert.ok(await page.locator("#token-file").evaluate(input => input.value === ""), "Disconnect clears file selection");

    // A genuine daemon 401 must forget an old saved token and retain its type in text.
    await page.locator("#token").fill(paths.token);
    await page.locator("#remember-token").check();
    await page.locator("#connect-form button[type=submit]").click();
    await connectedWithoutTokenInDom(page, server, paths.token);
    assert.equal(await hasSavedToken(page, paths.token), true);
    await page.reload({waitUntil:"networkidle"});
    await disconnected(page);
    await page.locator("#token").fill(wrongToken);
    const rejected = page.waitForResponse(response => new URL(response.url()).pathname.startsWith("/api/") && response.status() === 401);
    await page.locator("#connect-form button[type=submit]").click();
    const rejection = await rejected;
    assert.equal((await rejection.json()).error.code, "unauthorized");
    await page.locator("#error").waitFor({state:"visible"});
    assert.match(await page.locator("#error").textContent(), /unauthorized/);
    await disconnected(page);
    assert.equal(await hasNoSavedToken(page), true, "authentication rejection removes the prior saved token");
    assert.equal(await page.locator("#remember-token").isChecked(), false);
    assert.ok(await page.locator("#token").evaluate(input => input.value === ""));
    assert.ok(await page.locator("#error").evaluate((node, secret) => !node.textContent.includes(secret), wrongToken), "typed rejection does not echo the supplied token");
    assert.deepEqual(pageErrors, []);
  } finally {
    await context?.close();
    await browser?.close();
    await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});
