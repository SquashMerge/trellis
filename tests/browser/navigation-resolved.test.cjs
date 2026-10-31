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
    fixtureBinary = join(metadata.json().target_directory, "debug", process.platform === "win32" ? "trellis.exe" : "trellis");
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
      const fixture = seeded.json(), pin = fixture.revision;
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
          assert.ok(navigation.warnings.includes("multiple semantic producers for rust; showing syntax"));
          assert.match(await page.locator(".classes-context-menu").textContent(), /multiple semantic producers for rust; showing syntax/);
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
      const fixture = seeded.json(), pin = fixture.revision;
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
          if (mode === "multiple") assert.ok(dto.warnings.includes("multiple semantic producers for rust; showing syntax"));
          assert.match(await page.locator("#sequence-state").textContent(), / · Syntax only · (no semantic producer published|multiple semantic producers for rust); showing syntax · /);
        } else if (mode === "stale" || mode === "missing") {
          // #522: without a fresh binding for any of its calls the seed keeps the syntax
          // view, and the state names the reason.
          assert.ok(calls.every(call => call.callEvidence == null), JSON.stringify(calls));
          assert.equal(dto.semantic.tier, "syntax");
          assert.match(await page.locator("#sequence-state").textContent(), mode === "stale"
            ? / · Syntax only · Semantic evidence for flow\.rs is stale: it was imported for an earlier version of the file\. Showing syntax\. · /
            : / · Syntax only · No semantic binding covers this method's calls; showing syntax\. · /);
        } else {
          assert.ok(calls.every(call => call.callEvidence.boundaryReason === mode), JSON.stringify(calls));
          assert.match(await page.locator("#sequence-state").textContent(), / · Semantic bindings · semantic\.synthetic · /);
          if (mode === "external") {
            // #522: external calls are incidental: hidden by default, counted, and shown
            // by the toggle, which flips Show all.
            assert.ok(calls.every(call => call.hidden));
            assert.equal(await page.locator("#sequence-diagram [data-trellis-call-id]").count(), 0);
            const toggle = page.locator("#sequence-incidental");
            assert.equal(await toggle.textContent(), "Show 2 incidental calls");
            assert.equal(await toggle.getAttribute("aria-pressed"), "false");
            await toggle.click();
            await page.waitForFunction(() => document.querySelector("#sequence-incidental")?.textContent === "Hide 2 incidental calls" && /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""));
            assert.equal(await page.locator("#all-steps").isChecked(), true);
            assert.equal(await toggle.getAttribute("aria-pressed"), "true");
          } else assert.equal(await page.locator("#sequence-incidental").isHidden(), true);
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

// #450: an external call carries the package its SCIP symbol names, for in-context
// library lookup. It still has no target lane or declaration. This fixture's index never
// reaches a ready library catalog, so the inspector honestly offers no library action;
// the catalogued path is covered by the Architecture browser test and browse-ui.
test("an external call names its SCIP package and offers no library action without a catalog", {timeout:240000}, async () => {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  let server, browser, context;
  try {
    const seeded = await H.run(H.binary, ["navigation-fixture", "--workspace", paths.workspace, "--mode", "sequence-library"], {env:paths.env});
    const fixture = seeded.json(), pin = fixture.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    await page.locator(".file-pick").filter({hasText:"flow.rs"}).first().click();
    const selected = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.caller);
    await page.locator(".method-pick").filter({hasText:"measured"}).first().click();
    const dto = await (await selected).json();
    const calls = sequenceSteps(dto.steps).filter(step => fixture.calls.includes(step.callId));
    assert.equal(calls.length, 2);
    for (const call of calls) {
      assert.equal(call.callEvidence.boundaryReason, "external");
      assert.deepEqual(call.callEvidence.externalPackage, {manager:"cargo", name:"local-library", version:"0.1.0"});
      assert.equal(call.target, undefined);
    }
    assert.ok(dto.participants.every(row => !row.declaration || row.id === fixture.caller), "no external declaration lane");
    await page.locator("#sequence-diagram svg.pele").waitFor();
    // External calls are incidental (#522): show them first.
    await page.locator("#sequence-incidental").click();
    await page.waitForFunction(() => document.querySelector("#sequence-incidental")?.getAttribute("aria-pressed") === "true" && /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""));
    await page.locator(`#sequence-diagram [role=button][data-trellis-call-id="${fixture.calls[0]}"]`).first().click();
    await page.waitForFunction(() => /External boundary/.test(document.querySelector("#inspector-target")?.textContent || ""));
    assert.equal(await page.locator("#inspector-detail").getByRole("button", {name:"Open in library"}).count(), 0);
    assert.equal(await page.locator("#dock-library").isVisible(), false);
    assert.deepEqual(errors, []);
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});

// #522: participants come from fresh exact bindings; the direct callee's own body is
// expanded beneath its call and drawn on the callee's lane, in Pele and in the SVG
// fallback. Standard-library calls are incidental, and the inspector keeps the basis.
test("sequence draws resolved participants, expands direct callees and hides std calls", {timeout:240000}, async () => {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  let server, browser, context;
  try {
    const seeded = await H.run(H.binary, ["navigation-fixture", "--workspace", paths.workspace, "--mode", "sequence-expand"], {env:paths.env});
    const fixture = seeded.json(), pin = fixture.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    await page.locator(".file-pick").filter({hasText:"flow.rs"}).first().click();
    const selected = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === fixture.caller);
    await page.locator(".method-pick").filter({hasText:"measured"}).first().click();
    const dto = await (await selected).json();
    assert.deepEqual(dto.participants.map(row => row.id), [fixture.caller, fixture.callee, fixture.inner]);
    assert.deepEqual(dto.semantic, {tier:"semantic", producerId:"semantic.synthetic"});
    assert.deepEqual([dto.hiddenSteps, dto.incidentalSteps], [2, 2]);
    const steps = sequenceSteps(dto.steps), inner = steps.find(step => step.callId === fixture.calleeCalls[0]);
    assert.equal(inner.lane, fixture.callee);
    assert.equal(inner.target, fixture.inner);
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""));
    assert.match(await page.locator("#sequence-state").textContent(), / · Semantic bindings · semantic\.synthetic · 2 incidental steps hidden/);
    // Three lanes; the callee's call to `inner` leaves the callee's lane, not the seed's.
    assert.deepEqual(await page.locator("#sequence-diagram .pele-actor[data-trellis-target-id]").evaluateAll(rows => [...new Set(rows.map(row => row.dataset.trellisTargetId))]), [fixture.callee, fixture.inner]);
    const source = await page.evaluate(view => window.TrellisPeleSequence.project(view).source, dto);
    assert.match(source, /^S->>P1: /m);
    assert.match(source, /^P1->>P2: /m);
    assert.equal(await page.locator(`#sequence-diagram .pele-message[data-trellis-call-id="${fixture.calleeCalls[0]}"]`).getAttribute("data-trellis-target-id"), fixture.inner);
    // Std calls are hidden until the toggle shows them.
    for (const id of [fixture.calls[0], fixture.calleeCalls[1]]) assert.equal(await page.locator(`#sequence-diagram [data-trellis-call-id="${id}"]`).count(), 0);
    const toggle = page.locator("#sequence-incidental");
    assert.equal(await toggle.textContent(), "Show 2 incidental calls");
    await page.screenshot({path: process.env.TRELLIS_522_SHOTS ? `${process.env.TRELLIS_522_SHOTS}/fixture-expanded.png` : join(paths.temp, "shot.png")});
    // The inspector keeps the evidence and its captured basis.
    await page.locator(`#sequence-diagram .pele-edge-label[data-trellis-call-id="${fixture.calleeCalls[0]}"]`).click();
    await page.waitForFunction(() => /basis semantic\.synthetic/.test(document.querySelector("#inspector-evidence")?.textContent || ""));
    assert.match(await page.locator("#inspector-evidence").textContent(), /^Resolved declaration · direct · fresh · /);
    assert.match(await page.locator("#inspector-detail").textContent(), /"semanticBasis"/);
    await toggle.click();
    await page.waitForFunction(() => document.querySelector("#sequence-incidental")?.textContent === "Hide 2 incidental calls" && /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""));
    for (const id of [fixture.calls[0], fixture.calleeCalls[1]]) assert.ok(await page.locator(`#sequence-diagram [data-trellis-call-id="${id}"]`).count() > 0, id);
    assert.match(await page.locator("#sequence-state").textContent(), / · 0 incidental steps hidden/);
    // The SVG fallback draws the same lanes: the arrow to `inner` starts on the callee lane.
    const arrow = await page.evaluate(([view, id]) => {
      const box = document.createElement("div"); document.body.append(box);
      window.TrellisSequence.render(box, view, () => {}, new Set(), {});
      const lanes = [...box.querySelectorAll(".sequence-lifelines line")].map(line => line.getAttribute("x1"));
      const d = box.querySelector(`path.sequence-arrow[data-trellis-call-id="${id}"]`)?.getAttribute("d");
      box.remove();
      return {lanes, d};
    }, [dto, fixture.calleeCalls[0]]);
    assert.equal(arrow.lanes.length, 3);
    assert.ok(arrow.d.startsWith(`M ${arrow.lanes[1]} `), JSON.stringify(arrow));
    assert.ok(arrow.d.endsWith(` H ${arrow.lanes[2]}`), JSON.stringify(arrow));
    assert.deepEqual(errors, []);
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});
