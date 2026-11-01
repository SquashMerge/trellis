"use strict";
// Class diagrams for JavaScript (#452) through the production daemon and the
// unmodified shell: the published catalog reports its supported languages, and
// Pele class nodes and members map back to the exact response records.
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, rmSync, writeFileSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

// A real-shaped browser module: exported ES classes across files, plus a class
// expression, an object literal and a prototype assignment that stay out.
const widgets = `import { View } from "./view.js";
export class Widget extends View {
  #state = null;
  label = "";
  static instances = 0;
  constructor(label) { super(); this.label = label; Widget.instances++; }
  render(target) {
    if (target) target.textContent = this.label;
    return this.mount();
  }
  static of(label) { return new Widget(label); }
}
export default class Panel extends Widget { open() { return this.render(null); } }
export const Dialog = class extends View {};
export const helpers = { render() {} };
View.prototype.legacy = function () {};
`;
const view = "export class View {\n  mount() { return 1; }\n}\n";

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function withApp(files, run) {
  const [first, ...rest] = files;
  const paths = H.fixture(first[1], first[0]);
  for (const [name, text] of rest) {
    mkdirSync(dirname(join(paths.workspace, name)), {recursive:true});
    writeFileSync(join(paths.workspace, name), text);
  }
  let server, browser;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    const read = async (method, suffix, body) => {
      const response = await H.api(server.base, paths.token, method, H.scoped(server.rootKey, suffix), body);
      assert.equal(response.status, 200, JSON.stringify(response.data));
      return response.data;
    };
    browser = await chromium.launch();
    const page = await (await browser.newContext({viewport:{width:1440, height:900}})).newPage();
    const errors = [], requests = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text()); });
    page.on("request", request => { const url = new URL(request.url()); if (url.pathname.startsWith("/api/")) requests.push({url, body:request.method() === "POST" ? request.postDataJSON() : null}); });
    await H.connect(page, server, paths.token);
    await run({page, pin, read, requests, errors, reconnect:() => H.connect(page, server, paths.token)});
    assert.deepEqual(errors, [], "class Pele view runs under the real CSP");
  } finally {
    await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
const memberTexts = declaration => [...declaration.fields.map(f => `${f.name}${f.typeHint ? ": " + f.typeHint : ""}`),
  ...declaration.methods.map(m => `${m.name}()${m.typeHint ? " : " + m.typeHint : ""}`)];

test("published JavaScript classes render through Pele with members, menus and method sequences", {timeout:240000}, async () => {
  await withApp([["widgets.js", widgets], ["view.js", view]], async ({page, pin, read, requests}) => {
    const catalog = await read("GET", `classes?${new URLSearchParams({q:"", offset:"0", limit:"100", ...pin})}`);
    assert.ok(catalog.supportedLanguages.includes("javascript"), JSON.stringify(catalog.supportedLanguages));
    assert.deepEqual(catalog.items.map(item => item.qualifiedName).sort(), ["view.View", "widgets.Panel", "widgets.Widget"]);
    // #450: production JavaScript is class-catalog code, so the Classes tab earns its place.
    await page.locator("#view-classes").waitFor({state:"visible"});
    assert.deepEqual(await page.getByRole("tablist", {name:"Workspace views"}).getByRole("tab").allTextContents(), ["Architecture", "Treemap", "Sequence", "Classes"]);
    const widget = catalog.items.find(item => item.qualifiedName === "widgets.Widget");
    assert.equal(widget.language, "javascript");

    // Right-click the indexed file and choose Show class diagram.
    const file = page.locator(".file-pick[title='widgets.js']").first();
    await file.waitFor();
    await file.click({button:"right"});
    await page.getByRole("menuitem", {name:"Show class diagram"}).click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#classes-state")?.textContent || ""), null, {timeout:15000});
    const lookup = requests.filter(row => row.url.pathname.endsWith("/classes")).at(-1);
    assert.equal(lookup.url.searchParams.get("path"), "widgets.js");
    // The file's first class by qualified name opens directly.
    const panel = catalog.items.find(item => item.qualifiedName === "widgets.Panel");
    assert.equal(requests.filter(row => row.url.pathname.endsWith("/class-diagram")).at(-1).body.seed, panel.symbol.id);
    assert.equal(await page.locator(`#classes-diagram .pele-node[data-trellis-class-id="${panel.symbol.id}"] .pele-label`).textContent(), "Panel");

    // Search reaches the other class in the same file.
    await page.locator("#classes-query").fill("Widget");
    await page.locator("#classes-search").click();
    await page.locator(".classes-result").filter({hasText:"widgets.Widget"}).first().click();
    await page.waitForFunction(seed => /Pele rendered/.test(document.querySelector("#classes-state")?.textContent || "") &&
      !!document.querySelector(`#classes-diagram .pele-node[data-trellis-class-id="${seed}"]`), widget.symbol.id, {timeout:15000});
    const diagramRequest = requests.filter(row => row.url.pathname.endsWith("/class-diagram")).at(-1);
    assert.equal(diagramRequest.body.seed, widget.symbol.id);
    assert.deepEqual(diagramRequest.body.expectedRevision, pin);

    const node = page.locator(`#classes-diagram .pele-node[data-trellis-class-id="${widget.symbol.id}"]`);
    assert.equal(await page.locator("#classes-diagram .pele-node").count(), 1);
    assert.equal(await page.locator("#classes-diagram .pele-edge").count(), 0, "extends stays a terminal hint, never an arrow");
    assert.equal(await node.locator(".pele-label").textContent(), "Widget");
    assert.match(await node.locator(".pele-annotation").textContent(), /javascript · class/);
    assert.deepEqual(await node.locator(".pele-member").allTextContents(), memberTexts(widget));
    assert.deepEqual(memberTexts(widget), ["#state", "label", "instances", "constructor()", "render()", "of()"]);

    // A method member opens its exact measured sequence at the same pin.
    const render = widget.methods.find(method => method.name === "render");
    await page.locator(`.pele-member[data-trellis-symbol-id="${render.symbolId}"]`).click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""), null, {timeout:15000});
    const sequence = requests.filter(row => row.url.pathname.endsWith("/sequence")).at(-1);
    assert.equal(sequence.body.seed, render.symbolId);
    assert.deepEqual(sequence.body.expectedRevision, pin);
  });
});

const rustModel = `pub struct Account {
    pub id: u64,
    balance: i64,
}
pub trait Ledger {
    fn post(&mut self, account: &Account) -> i64;
}
`;
const rustOps = `use crate::model::{Account, Ledger};
impl Account {
    pub fn balance(&self) -> i64 {
        if self.balance > 0 { self.balance } else { 0 }
    }
}
pub struct Memory;
impl Ledger for Memory {
    fn post(&mut self, account: &Account) -> i64 { account.balance() }
}
`;

test("published Rust types merge impl methods across files and render through Pele", {timeout:240000}, async () => {
  await withApp([["Cargo.toml", "[package]\nname = \"bank\"\nversion = \"0.1.0\"\n"], ["src/lib.rs", "pub mod model;\npub mod ops;\n"],
    ["src/model.rs", rustModel], ["src/ops.rs", rustOps]], async ({page, pin, read, requests}) => {
    const catalog = await read("GET", `classes?${new URLSearchParams({q:"", offset:"0", limit:"100", ...pin})}`);
    assert.ok(catalog.supportedLanguages.includes("rust"), JSON.stringify(catalog.supportedLanguages));
    assert.deepEqual(catalog.items.map(item => item.qualifiedName).sort(), ["model::Account", "model::Ledger", "ops::Memory"]);
    const account = catalog.items.find(item => item.qualifiedName === "model::Account");
    assert.deepEqual(account.methods.map(method => [method.name, method.path]), [["balance", "src/ops.rs"]]);

    // Right-click the model file: its first type opens with members from ops.rs.
    await page.locator(".directory-pick[title='src/']").first().click();
    const file = page.locator(".file-pick[title='src/model.rs']").first();
    await file.waitFor();
    await file.click({button:"right"});
    await page.getByRole("menuitem", {name:"Show class diagram"}).click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#classes-state")?.textContent || ""), null, {timeout:15000});
    assert.equal(requests.filter(row => row.url.pathname.endsWith("/class-diagram")).at(-1).body.seed, account.symbol.id);
    const node = page.locator(`#classes-diagram .pele-node[data-trellis-class-id="${account.symbol.id}"]`);
    assert.match(await node.locator(".pele-annotation").textContent(), /rust · struct/);
    assert.equal(await page.locator("#classes-diagram .pele-edge").count(), 0);
    assert.deepEqual(await node.locator(".pele-member").allTextContents(), memberTexts(account));
    assert.deepEqual(memberTexts(account), ["id: u64", "balance: i64", "balance() : i64"]);

    // The trait is an interface box with its methods.
    await page.locator("#classes-query").fill("Ledger");
    await page.locator("#classes-search").click();
    await page.locator(".classes-result").filter({hasText:"model::Ledger"}).first().click();
    const ledger = catalog.items.find(item => item.qualifiedName === "model::Ledger");
    await page.waitForFunction(seed => /Pele rendered/.test(document.querySelector("#classes-state")?.textContent || "") &&
      !!document.querySelector(`#classes-diagram .pele-node[data-trellis-class-id="${seed}"]`), ledger.symbol.id, {timeout:15000});
    assert.match(await page.locator(`#classes-diagram .pele-node[data-trellis-class-id="${ledger.symbol.id}"] .pele-annotation`).textContent(), /rust · trait/);

    // A merged method member opens its measured sequence in ops.rs.
    await page.locator("#classes-query").fill("Account");
    await page.locator("#classes-search").click();
    await page.locator(".classes-result").filter({hasText:"model::Account"}).first().click();
    await page.waitForFunction(seed => !!document.querySelector(`#classes-diagram .pele-node[data-trellis-class-id="${seed}"]`), account.symbol.id, {timeout:15000});
    const balance = account.methods[0];
    await page.locator(`.pele-member[data-trellis-symbol-id="${balance.symbolId}"]`).click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""), null, {timeout:15000});
    const sequence = requests.filter(row => row.url.pathname.endsWith("/sequence")).at(-1);
    assert.equal(sequence.body.seed, balance.symbolId);
    assert.deepEqual(sequence.body.expectedRevision, pin);
  });
});

test("a reload shows the Classes tab this browser remembers while metrics are still out", {timeout:240000}, async () => {
  await withApp([["widgets.js", widgets]], async ({page, reconnect}) => {
    await page.locator("#view-classes").waitFor({state:"visible"});
    let release;
    const held = new Promise(resolve => { release = resolve; });
    await page.route("**/metrics?**", async route => { await held; await route.continue(); });
    const out = page.waitForRequest(/\/metrics\?/);
    await reconnect();
    await out;
    assert.equal(await page.locator("#view-classes").isVisible(), true, "the hint shows before the answer");
    assert.doesNotMatch(await page.locator("#metrics-state").textContent(), /Indexed snapshot ·/);
    release();
    await page.waitForFunction(() => /Indexed snapshot ·/.test(document.querySelector("#metrics-state")?.textContent || ""));
    assert.equal(await page.locator("#view-classes").isVisible(), true, "and the answer confirms it");
    await page.unrouteAll({behavior:"wait"});
  });
});

// #450: the class catalog indexes test classes too, but only production code in a
// supported language shows the tab, with or without tests in the metric population.
test("Classes stays hidden when the checkout's class-catalog code is test-only", {timeout:240000}, async () => {
  await withApp([["widgets.test.js", widgets], ["view.test.js", view]], async ({page, pin, read, requests}) => {
    const catalog = await read("GET", `classes?${new URLSearchParams({q:"", offset:"0", limit:"100", ...pin})}`);
    assert.ok(catalog.supportedLanguages.includes("javascript"));
    assert.ok(catalog.items.length > 0, "test classes are catalogued");
    const tabs = page.getByRole("tablist", {name:"Workspace views"}).getByRole("tab");
    await page.waitForFunction(() => /Indexed snapshot/.test(document.querySelector("#metrics-state")?.textContent || ""));
    assert.equal(await page.locator("#view-classes").isVisible(), false);
    assert.deepEqual(await tabs.allTextContents(), ["Architecture", "Treemap", "Sequence"]);
    assert.equal(requests.some(row => row.url.pathname.endsWith("/classes")), false, "no production code: the class catalog is not asked");
    await H.reveal(page, "#metric-include-tests");
    await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => /all indexed callables/.test(document.querySelector("#metrics-state")?.textContent || ""));
    assert.equal(await page.locator("#view-classes").isVisible(), false, "test code never earns the tab");
  });
});
