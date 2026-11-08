// The mod's logic in Node, with the browser's own adapter and Pele (no install needed):
//   node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/mod.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { DOMParser, serialize } from "../src/dom.js";
import { draw, hit, measure, summarize } from "../src/diagram.js";
import { TrellisError, architecturePayload, createClient, indexLabel, pinOf } from "../src/client.js";
import { state } from "./stubs/herdr.js";

const { fixture } = createRequire(import.meta.url)("../../tests/fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
const view = () => {
  const v = fixture();
  for (const m of v.modules) m.declarationCount = 1;
  return { ...v, rulesFile: { contentHash: "abc" }, references: { coverage: [] } };
};
const theme = { background: "#000001", foreground: "#000002", muted: "#000003", input: "#000004", surface: "#000005", raised: "#000006",
  border: "#000007", accent: "#000008", ansi0: "#000009", ansi1: "#00000a", ansi3: "#00000b", ansi9: "#00000c", ansi11: "#00000d" };

test("the SVG DOM parses Pele-shaped markup, selects, and serializes text safely", () => {
  const root = new DOMParser().parseFromString('<svg xmlns="http://www.w3.org/2000/svg"><g class="pele-node a" data-id="x"><rect x="1"/><text>a &amp; b</text></g></svg>', "image/svg+xml").documentElement;
  assert.equal(root.namespaceURI, "http://www.w3.org/2000/svg");
  assert.equal(root.querySelectorAll(".pele-node, .pele-cluster").length, 1);
  const g = root.querySelector("[data-id]");
  assert.equal(g.querySelector(":scope > rect").getAttribute("x"), "1");
  assert.equal(g.querySelector("text").textContent, "a & b");
  g.querySelector("text").textContent = '<script>"x"</script>';
  assert.ok(serialize(root).includes("&lt;script&gt;\"x\"&lt;/script&gt;"));
  assert.equal(new DOMParser().parseFromString("<svg><g></svg>", "image/svg+xml").documentElement.localName, "parsererror");
  assert.throws(() => root.querySelector("g text"), /Unsupported selector/);
});

test("text is measured with Pele's own headless widths", () => {
  assert.equal(measure(" "), 278 * 14 / 1000);
  assert.equal(measure("中"), 14);
  assert.equal(measure("é"), measure("e"));
});

test("the diagram is the browser adapter's drawing, themed, with a box per control", async () => {
  const out = await draw(view(), { width: 900, open: new Map(), theme, highlight: null });
  assert.match(out.svg, /^<svg xmlns="http:\/\/www.w3.org\/2000\/svg" class="pele pele-block trellis-architecture-svg"/);
  assert.ok(out.width > 0 && out.width <= 900 && out.height > 0);
  // No brand colour survives: every fill and stroke is a theme colour or none.
  const colours = new Set([...out.svg.matchAll(/(?:fill|stroke)="([^"]+)"/g)].map((m) => m[1]));
  for (const colour of colours) assert.ok(colour === "none" || Object.values(theme).includes(colour), `unthemed colour ${colour}`);
  assert.equal(out.boxes.filter((b) => b.kind === "L").length, 5);
  assert.equal(out.boxes.filter((b) => b.kind === "E").length, 4, "test edges stay hidden, as in the browser");
  const arrow = out.boxes.find((b) => b.kind === "E");
  assert.equal(hit(out.boxes, arrow.x + 1, arrow.y + 1).id, arrow.id, "the smallest box under the pointer wins");
  assert.equal(hit(out.boxes, -5, -5), null);
});

test("opening a band and highlighting a module chip change only the drawing", async () => {
  const v = view(), before = JSON.stringify(v);
  const closed = await draw(v, { width: 900, open: new Map(), theme, highlight: null });
  const open = await draw(v, { width: 900, open: new Map([["UI", true]]), theme, highlight: null });
  assert.ok(open.boxes.some((b) => b.kind === "M"), "an open band shows file chips");
  assert.ok(!closed.boxes.some((b) => b.kind === "M"));
  const group = summarize(v).moduleGroups(v.modules.filter((m) => m.layer === "UI"))[0];
  const marked = await draw(v, { width: 900, open: new Map(), theme, highlight: ["UI", group.name] });
  assert.ok(marked.svg.includes(`stroke="${theme.accent}"`));
  assert.equal(JSON.stringify(v), before);
});

test("an invalid payload is a typed error, never a partial drawing", async () => {
  const v = view(); v.modules.push(v.modules[0]);
  await assert.rejects(draw(v, { width: 900, open: new Map(), theme, highlight: null }), (e) => /duplicate Architecture module/.test(e.message) && e.code === "unsupported");
});

test("the client accepts loopback only and types every failure", async () => {
  assert.throws(() => createClient({ url: "http://example.com:8877" }), (e) => e.code === "invalid_url");
  assert.throws(() => createClient({ url: "https://127.0.0.1:8877" }), (e) => e.code === "invalid_url");
  state.home = "/nonexistent-home";
  await assert.rejects(createClient({}).checkouts(), (e) => e.code === "token_missing" && !/[0-9a-f]{64}/.test(e.message));

  const token = "a".repeat(64), seen = [];
  const file = new URL("./fixture-token.txt", import.meta.url).pathname;
  const { writeFileSync, rmSync } = await import("node:fs");
  writeFileSync(file, `${token}\n`);
  const realFetch = globalThis.fetch;
  try {
    globalThis.fetch = async (url, options) => { seen.push([url, options.headers.Authorization]); return { ok: false, status: 401, json: async () => ({ error: { code: "unauthorized", message: "Bearer authentication required" } }) }; };
    await assert.rejects(createClient({ tokenFile: file }).checkouts(), (e) => e instanceof TrellisError && e.code === "unauthorized" && !e.message.includes(token));
    assert.deepEqual(seen[0], ["http://127.0.0.1:8877/api/checkouts", `Bearer ${token}`]);
    globalThis.fetch = async () => { throw new Error("connection refused"); };
    await assert.rejects(createClient({ tokenFile: file }).checkouts(), (e) => e.code === "daemon_unreachable");
  } finally { globalThis.fetch = realFetch; rmSync(file); }
});

test("Architecture answers are checked against the pin, as the browser checks them", () => {
  const pin = pinOf({ indexGeneration: "12345678-1234-4123-8123-123456789abc", indexRevision: 1 });
  assert.throws(() => architecturePayload({ payload: { ...view(), indexRevision: 2 } }, pin), (e) => e.code === "revision_conflict");
  assert.throws(() => architecturePayload({ payload: view() }, pin), (e) => e.code === "invalid_response");
  assert.equal(architecturePayload({ payload: { state: "building", indexGeneration: pin.indexGeneration, indexRevision: 1 } }, pin).state, "building");
  assert.equal(indexLabel({ state: "available", revision: pin, catchingUp: false }).text, "Fresh · rev 12345678:1");
  assert.equal(indexLabel({ state: "unavailable", indexError: "root_changed" }).text, "Unavailable · root_changed");
});
