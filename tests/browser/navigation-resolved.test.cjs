"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {rmSync} = require("node:fs");
const {join, resolve} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

// Publication and daemon must use identical executable bytes, so startup retains
// the exact validated pin. Other browser suites use the normal production-feature build.
test.before(async () => {
  let fixtureBinary = process.env.TRELLIS_NAVIGATION_FIXTURE_BINARY;
  if (!fixtureBinary) {
    const metadata = await H.run("cargo", ["metadata", "--no-deps", "--format-version", "1"]);
    await H.run("cargo", ["build", "--locked", "--features", "test-support", "--bin", "trellis"], {timeout:900000});
    fixtureBinary = join(JSON.parse(metadata.stdout).target_directory, "debug", process.platform === "win32" ? "trellis.exe" : "trellis");
  }
  process.env.TRELLIS_SUITE_BINARY = resolve(fixtureBinary);
  await H.setup();
}, {timeout:960000});
test.after(async () => H.cleanup());

for (const mode of ["zero", "sole", "multiple", "unresolved"]) {
  test(`source navigation uses sole published semantic evidence: ${mode}`, {timeout:240000}, async () => {
    const paths = H.fixture("", "placeholder.txt");
    rmSync(paths.source);
    let server, browser, context;
    try {
      const seeded = await H.run(H.binary, ["navigation-fixture", "--workspace", paths.workspace, "--mode", mode], {env:paths.env});
      const fixture = JSON.parse(seeded.stdout), pin = fixture.revision;
      server = await H.startDaemon(paths, pin.indexRevision);
      browser = await chromium.launch();
      context = await browser.newContext();
      const page = await context.newPage(), errors = [], requests = [];
      page.on("pageerror", error => errors.push(error.message));
      page.on("request", request => {
        if (new URL(request.url()).pathname.startsWith("/api/")) requests.push(request);
      });
      await H.connect(page, server, paths.token);
      await page.locator(".file-pick").filter({hasText:"flow.rs"}).first().click();
      await page.locator(".method-pick").filter({hasText:"measured"}).first().click();
      await page.locator("#method-source").click();
      await page.locator("#source .source-line").first().waitFor();
      const line = page.locator("#source .source-line").filter({hasText:"lexical_decoy();"});
      const sourceBefore = await page.locator("#source").textContent();
      const highlightBefore = await page.locator("#source .highlight .line-number").allTextContents();
      const navigationPromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/navigation"));
      await line.click({button:"right"});
      const response = await navigationPromise, navigation = await response.json();
      assert.equal(response.status(), 200, JSON.stringify(navigation));
      assert.deepEqual(navigation.revision, pin);
      const request = response.request().postDataJSON();
      assert.equal(request.semanticProducerPolicy, "solePublished");
      assert.equal(request.semanticProducerId, undefined);
      assert.deepEqual(request.expectedRevision, pin);
      assert.equal(request.path, "flow.rs"); assert.equal(request.line, 2);
      const calls = navigation.targets.filter(target => target.reason === "call");
      if (mode === "sole") {
        assert.equal(calls.length, 1);
        assert.equal(calls[0].symbol.id, fixture.callee);
        assert.equal(calls[0].symbol.name, "actual_target");
        assert.equal(calls[0].symbol.path, "target.rs");
        assert.equal(calls[0].matchKind, "resolved");
        assert.notEqual(calls[0].symbol.id, fixture.decoy, "spelling is not a semantic target");
        const callee = page.getByRole("menuitem", {name:/^Sequence of callee · resolved · actual_target/});
        await callee.waitFor();
        const selectedPromise = page.waitForResponse(row => new URL(row.url()).pathname.endsWith("/sequence") && row.request().postDataJSON()?.seed === fixture.callee);
        await callee.click();
        const selectedResponse = await selectedPromise, selected = await selectedResponse.json();
        assert.equal(selectedResponse.status(), 200, JSON.stringify(selected));
        assert.equal(selected.seed.id, fixture.callee); assert.deepEqual(selected.revision, pin);
        await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id, fixture.callee);
        await page.goBack();
        await page.waitForFunction(text => document.querySelector("#source")?.textContent === text && !document.querySelector("#workspace-source-panel")?.hidden, sourceBefore);
        assert.deepEqual(await page.locator("#source .highlight .line-number").allTextContents(), highlightBefore);
        assert.match(await page.locator(".method-pick.selected").textContent(), /measured/);
      } else {
        assert.deepEqual(calls, [], "no guessing from a source spelling");
        assert.equal(await page.getByRole("menuitem", {name:/Sequence of callee/}).count(), 0);
        assert.ok(navigation.targets.some(target => target.reason === "enclosing" && target.symbol.id === fixture.caller));
        await page.getByRole("menuitem", {name:/^Sequence · enclosing · syntax · measured/}).waitFor();
        if (mode === "multiple") {
          assert.ok(navigation.warnings.includes("multiple semantic producers; showing syntax"));
          assert.match(await page.locator(".classes-context-menu").textContent(), /multiple semantic producers; showing syntax/);
        }
      }
      assert.equal(requests.some(row => row.method() === "POST" && new URL(row.url()).pathname.endsWith("/index")), false);
      assert.deepEqual(errors, []);
    } finally {
      await context?.close(); await browser?.close(); await H.stopDaemon(server);
      rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
    }
  });
}
