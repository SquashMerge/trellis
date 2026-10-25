"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync,rmSync,writeFileSync} = require("node:fs");
const {dirname,resolve} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const source = "function measured(input) {\n  validate(input);\n  return persist(input);\n}\n";
const measurements = [];
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

// Read rendered colors, including transparent ancestor surfaces, rather than
// comparing token strings that might never reach the real control.
async function contrast(page, selector, {foreground = "color", backing, border = false} = {}) {
  return page.locator(selector).first().evaluate((node, options) => {
    const rgba = value => {
      const parts = value.match(/[\d.]+/g)?.map(Number);
      if (!parts || parts.length < 3) throw new Error(`Unsupported computed color: ${value}`);
      return [...parts.slice(0,3), parts[3] ?? 1];
    };
    const over = (top, bottom) => [...top.slice(0,3).map((v,i) => v*top[3] + bottom[i]*(1-top[3])), 1];
    const surface = element => {
      const chain = [];
      for (let current = element; current; current = current.parentElement) chain.unshift(current);
      return chain.reduce((color,current) => over(rgba(getComputedStyle(current).backgroundColor), color), [255,255,255,1]);
    };
    const luminance = color => color.slice(0,3).map(v => v/255).map(v => v <= .04045 ? v/12.92 : ((v+.055)/1.055)**2.4)
      .reduce((sum,v,i) => sum + v*[.2126,.7152,.0722][i],0);
    const ratio = (a,b) => (Math.max(luminance(a),luminance(b))+.05)/(Math.min(luminance(a),luminance(b))+.05);
    const style = getComputedStyle(node);
    let background = surface(node);
    if (options.backing) {
      const backingStyle = getComputedStyle(document.querySelector(options.backing));
      const fill = rgba(backingStyle.fill);
      fill[3] *= Number(backingStyle.fillOpacity) * Number(backingStyle.opacity);
      background = over(fill,background);
    }
    const foreground = over(rgba(style[options.border ? "borderTopColor" : options.foreground]),background);
    return {foreground, background, ratio:ratio(foreground,background),
      outsideRatio:options.border ? ratio(foreground,surface(node.parentElement)) : undefined};
  }, {foreground, backing, border});
}
async function readable(page, selector, options) {
  const result = await contrast(page, selector, options);
  assert.ok(result.ratio >= 4.5, `${selector} text contrast ${JSON.stringify(result)}`);
  measurements.push({kind:"text",selector,...result});
}
async function focusRing(page, selector) {
  await page.locator(selector).first().focus();
  const ring = await page.locator(selector).first().evaluate(node => {
    const s = getComputedStyle(node);
    return {visible:node.matches(":focus-visible"), width:s.outlineWidth, style:s.outlineStyle, color:s.outlineColor};
  });
  assert.equal(ring.visible,true,`${selector} must show keyboard focus`);
  assert.equal(ring.width,"2px");
  assert.equal(ring.style,"solid");
  assert.equal(ring.color,"rgb(171, 165, 255)");
}
async function loadedFonts(page) {
  await page.evaluate(async () => {
    await Promise.all([document.fonts.load('400 13px "Inter"'), document.fonts.load('600 21px "Manrope"'), document.fonts.load('400 12px "JetBrains Mono"')]);
    await document.fonts.ready;
  });
  const fonts = await page.evaluate(() => [...document.fonts].filter(f => f.status === "loaded").map(f => f.family.replace(/["']/g,"")));
  for (const family of ["Inter","Manrope","JetBrains Mono"]) assert.ok(fonts.includes(family),`font did not load: ${family}; ${fonts}`);
}

test("real shell loads local brand fonts, preserves Connect and source, and renders accessible selected/focus states", {timeout:240000}, async () => {
  const paths = H.fixture(source,"flow.js");
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary,["index","--workspace",paths.workspace],{env:paths.env,timeout:120000});
    const pin = JSON.parse(indexed.stdout).status.revision;
    server = await H.startDaemon(paths,pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440,height:900}});
    const page = await context.newPage();
    const errors = [], fontResponses = [];
    page.on("pageerror",error => errors.push(error.message));
    page.on("console",message => { if (message.type() === "error") errors.push(message.text()); });
    page.on("response",response => { if (new URL(response.url()).pathname.endsWith(".woff2")) fontResponses.push({url:response.url(),status:response.status()}); });
    await page.goto(`${server.base}/`,{waitUntil:"networkidle"});
    await loadedFonts(page);
    for (const selector of [".brand-mark", ".connection-logo"]) {
      assert.equal(await page.locator(selector).evaluate(img => img.complete && img.naturalWidth > 0),true,`${selector} loaded from daemon`);
    }
    assert.equal(await page.locator(".brand").getAttribute("aria-label"),"Trellis home");
    for (const selector of ["#token","#remember-token","#token-file","#connect-form button"]) assert.equal(await page.locator(selector).isVisible(),true,selector);
    assert.match(await page.locator("body").evaluate(node => getComputedStyle(node).fontFamily),/^Inter(?:,|$)/);
    assert.match(await page.locator("h1").evaluate(node => getComputedStyle(node).fontFamily),/^Manrope(?:,|$)/);
    await page.keyboard.press("Tab");
    await focusRing(page,"#token");
    for (const selector of ["#connect-form button", "#connect-form label", "#connect-form .muted", "#notice"]) await readable(page,selector);
    const border = await contrast(page,"#token",{border:true});
    assert.ok(border.ratio >= 3 && border.outsideRatio >= 3,`input boundary contrast ${JSON.stringify(border)}`);
    measurements.push({kind:"control-boundary",selector:"#token",...border});
    // Check the real disconnected layout too, without a second daemon or fake DOM.
    await page.setViewportSize({width:390,height:844});
    const bounds = await page.locator("#token").boundingBox();
    assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= 390,"mobile token input remains in viewport");
    assert.equal(await page.locator("#connect-form button").isVisible(),true);
    await page.setViewportSize({width:1440,height:900});
    await H.connect(page,server,paths.token);
    for (const selector of ["#checkout-select","#refresh"]) {
      await readable(page,selector);
      const boundary = await contrast(page,selector,{border:true});
      assert.ok(boundary.ratio >= 3 && boundary.outsideRatio >= 3,`${selector} boundary contrast ${JSON.stringify(boundary)}`);
      measurements.push({kind:"control-boundary",selector,...boundary});
    }
    await page.locator(".file-pick").filter({hasText:"flow.js"}).first().click();
    await page.locator(".method-pick").filter({hasText:"measured"}).first().click();
    await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent?.includes("Pele rendered"));
    const note = page.locator(".pele-note[data-trellis-step-id]").first();
    await note.focus(); await page.keyboard.press("Enter");
    await focusRing(page,".pele-note[data-trellis-step-id][aria-pressed=true]");
    for (const selector of [".method-pick.selected", "#view-sequence", "#inspector-target", "#inspector-open-source", "#sequence-state"]) await readable(page,selector);
    await readable(page,".pele-note[aria-pressed=true] text",{foreground:"fill",backing:".pele-note[aria-pressed=true] rect"});
    await page.locator("#inspector-open-source").click();
    await page.waitForFunction(() => document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
    assert.equal(await page.locator("#source .source-line").evaluateAll(lines => lines.map(line => line.textContent.replace(/^\d+/,"")).join("\n")),source);
    assert.match(await page.locator("#source").evaluate(node => getComputedStyle(node).fontFamily),/^"?JetBrains Mono"?(?:,|$)/);
    await readable(page,"#source .source-line.highlight");
    await loadedFonts(page);
    for (const file of ["inter-latin.woff2","manrope-latin.woff2","jetbrains-mono-latin.woff2"]) {
      assert.ok(fontResponses.some(response => response.url === `${server.base}/fonts/${file}` && response.status === 200),`local font response missing: ${file}; ${JSON.stringify(fontResponses)}`);
    }
    assert.deepEqual(errors,[],"no CSP, font, or application errors");
    if (process.env.TRELLIS_BRAND_REPORT) {
      const report = resolve(process.env.TRELLIS_BRAND_REPORT);
      mkdirSync(dirname(report),{recursive:true});
      writeFileSync(report,JSON.stringify({revision:pin,fontResponses,measurements,errors},null,2)+"\n");
    }
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
});
