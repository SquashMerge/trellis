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

function sequenceSteps(roots) {
  return roots.flatMap(step => [step, ...sequenceSteps(step.children || []), ...sequenceSteps(step.alternate || [])]);
}

for (const mode of ["sole", "constructor", "zero", "multiple", "unresolved", "ambiguous", "stale", "dispatch", "external", "missing"]) {
  test(`sequence uses each published call occurrence without guessing: ${mode}`, {timeout:240000}, async () => {
    const paths = H.fixture("", "placeholder.txt");
    rmSync(paths.source);
    let server, browser, context;
    try {
      const seeded = await H.run(H.binary, ["navigation-fixture", "--workspace", paths.workspace, "--mode", `sequence-${mode}`], {env:paths.env});
      const fixture = JSON.parse(seeded.stdout), pin = fixture.revision;
      assert.equal(fixture.calls.length, 2);
      assert.notEqual(fixture.calls[0], fixture.calls[1]);
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
      const selectedPromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.caller);
      await page.locator(".method-pick").filter({hasText:"measured"}).first().click();
      const selectedResponse = await selectedPromise, dto = await selectedResponse.json();
      assert.equal(selectedResponse.status(), 200, JSON.stringify(dto));
      assert.deepEqual(dto.revision, pin);
      assert.equal(dto.seed.id, fixture.caller);
      const calls = sequenceSteps(dto.steps).filter(step => fixture.calls.includes(step.callId));
      assert.deepEqual(calls.map(step => step.callId), fixture.calls, "every occurrence survives graph grouping");
      await page.locator("#sequence-diagram svg.pele").waitFor();
      assert.match(await page.locator("#sequence-state").textContent(), /Pele rendered/);
      if (mode === "sole") {
        const participant = dto.participants.find(row => row.id === calls[0].target);
        assert.equal(participant.declaration.id, fixture.callee);
        assert.equal(participant.declaration.name, "actual_target");
        assert.equal(participant.declaration.path, "target.rs");
        assert.notEqual(participant.declaration.id, fixture.decoy);
        for (const call of calls) {
          assert.equal(call.target, participant.id);
          assert.equal(call.callEvidence.boundaryReason, "none");
          assert.equal(call.callEvidence.resolution, "resolved");
          assert.equal(call.callEvidence.freshness, "fresh");
          assert.equal(call.callEvidence.dispatch, "direct");
        }
        const messages = page.locator("#sequence-diagram .pele-edge-label[data-trellis-call-id]");
        assert.deepEqual(await messages.evaluateAll(rows => rows.map(row => row.dataset.trellisCallId)), fixture.calls);
        assert.deepEqual(await page.locator("#sequence-diagram .pele-actor[data-trellis-target-id]").evaluateAll(rows => [...new Set(rows.map(row => row.dataset.trellisTargetId))]), [participant.id]);
        await messages.first().click();
        assert.match(await page.locator("#call-inspector").innerText(), /actual_target/);
        assert.doesNotMatch(await page.locator("#inspector-target").innerText(), /no verified target/);
        const lane = page.getByRole("button", {name:"Open callee sequence: actual_target", exact:true}).first();
        await lane.focus();
        const targetPromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.callee);
        await page.keyboard.press("Enter");
        const targetResponse = await targetPromise, target = await targetResponse.json();
        assert.equal(targetResponse.status(), 200, JSON.stringify(target));
        assert.deepEqual(target.revision, pin);
        assert.equal(target.seed.id, fixture.callee);
        await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id, fixture.callee);
        await page.goBack();
        await page.locator("#sequence-diagram .pele-edge-label[data-trellis-call-id]").first().waitFor();
        assert.match(await page.locator(".method-pick.selected").textContent(), /measured/);
        const mouseTarget = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.callee);
        await page.getByRole("button", {name:"Open callee sequence: actual_target", exact:true}).first().click();
        assert.deepEqual((await (await mouseTarget).json()).revision, pin);
        // The Files mini sequence uses the same exact target and navigation owner.
        await page.locator("#section-files").click();
        await page.waitForFunction(() => document.body.classList.contains("files-section"));
        await page.locator("#file-tree .file-pick").filter({hasText:"flow.rs"}).first().click();
        await page.locator(`.files-symbol-pick[data-symbol-id="${fixture.caller}"]`).click();
        const miniMessages = page.locator("#files-mini-sequence .pele-edge-label[data-trellis-call-id]");
        await miniMessages.first().waitFor();
        assert.deepEqual(await miniMessages.evaluateAll(rows => rows.map(row => row.dataset.trellisCallId)), fixture.calls);
        const miniTarget = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.callee);
        await page.locator("#files-mini-sequence").getByRole("button", {name:"Open callee sequence: actual_target", exact:true}).first().click();
        assert.deepEqual((await (await miniTarget).json()).revision, pin);
        await page.goBack();
        await page.waitForFunction(() => document.body.classList.contains("files-section") && document.querySelector("#files-symbol-title")?.textContent.includes("measured"));
        await page.locator("#files-mini-sequence .pele-edge-label[data-trellis-call-id]").first().waitFor();
        assert.deepEqual(await page.locator("#files-mini-sequence .pele-edge-label[data-trellis-call-id]").evaluateAll(rows => rows.map(row => row.dataset.trellisCallId)), fixture.calls);
        assert.match(await page.locator("#source-path").textContent(), /flow\.rs/);
        assert.ok((await page.locator("#source").textContent()).includes("lexical_decoy();"));
      } else if (mode === "constructor") {
        const participant = dto.participants.find(row => row.id === calls[0].target);
        assert.equal(participant.declaration.id, fixture.callee);
        assert.equal(participant.declaration.path, "target.rs");
        assert.equal(participant.declaration.kind, "class");
        assert.ok(calls.every(call => call.target === participant.id && call.callEvidence.dispatch === "constructor"));
        assert.equal(await page.getByRole("button", {name:/Open callee sequence: actual_target/}).count(), 0);
        const sourcePromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/source") && new URL(response.url()).searchParams.get("path") === "target.rs");
        await page.getByRole("button", {name:"Read declaration source: actual_target", exact:true}).first().click();
        const sourceResponse = await sourcePromise, source = await sourceResponse.json();
        assert.equal(sourceResponse.status(), 200, JSON.stringify(source));
        assert.deepEqual(source.revision, pin);
        assert.ok((await page.locator("#source").textContent()).includes("pub struct actual_target;"));
        assert.equal(requests.some(row => new URL(row.url()).pathname.endsWith("/sequence") && row.postDataJSON()?.seed === fixture.callee), false);
      } else {
        assert.ok(calls.every(call => call.target == null), "nonresolved evidence never fabricates a target");
        assert.equal(await page.locator("#sequence-diagram .pele-message").count(), 0);
        assert.equal(await page.getByRole("button", {name:/Open callee sequence:/}).count(), 0);
        if (mode === "zero" || mode === "multiple") {
          assert.ok(calls.every(call => call.callEvidence == null));
          if (mode === "multiple") assert.ok(dto.warnings.includes("multiple semantic producers; showing syntax"));
        } else {
          const reason = mode === "missing" ? "missingEvidence" : mode;
          assert.ok(calls.every(call => call.callEvidence.boundaryReason === reason), JSON.stringify(calls));
          const visible = await page.locator("#sequence-diagram").innerText();
          const label = {missing:/missing|unavailable|no semantic/i, stale:/stale/i, dispatch:/dispatch/i, external:/external/i, ambiguous:/ambiguous/i, unresolved:/unresolved|unbound/i}[mode];
          assert.match(visible, label);
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
