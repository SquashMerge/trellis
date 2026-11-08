// Runs the real mod (src/main.js) in Node against a live loopback daemon, with stand-ins for
// the host modules, and prints what its pane would show after the data settles.
//
//   node test/harness.mjs --url http://127.0.0.1:8877 --token-file ~/.config/trellis/serve.token \
//        [--cwd <focused folder>] [--select <relative path>] [--click <element id>] [--press E|L] [--transport exec] [--width 900]
//
// It checks the data path (token, checkouts, status, Architecture, drawing, followed file). It
// is not a picture of the pane: look at the real one with `herdr-app shot`.
import { build } from "esbuild";
import { existsSync, mkdtempSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { webPlugin } from "../web-plugin.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => { const i = process.argv.indexOf(name); return i >= 0 ? process.argv[i + 1] : fallback; };
const out = join(mkdtempSync(join(tmpdir(), "trellis-mod-")), "main.mjs");
await build({
  entryPoints: [join(here, "..", "src", "main.js")], bundle: true, platform: "node", format: "esm", outfile: out, logLevel: "warning",
  plugins: [webPlugin], alias: { "gpui-kit": join(here, "stubs", "gpui.js"), "gpui-base": join(here, "stubs", "gpui.js"), herdr: join(here, "stubs", "herdr.js") },
});
const { default: Trellis } = await import(out);
const { state } = await import(join(here, "stubs", "herdr.js"));
const appHome = mkdtempSync(join(tmpdir(), "trellis-home-"));
Object.assign(state, { appHome, props: { url: arg("--url", "http://127.0.0.1:8877"), tokenFile: arg("--token-file"), transport: arg("--transport") },
  // The focused terminal's folder: the checkout the mod opens without being told.
  focused: arg("--cwd") ? { cwd: arg("--cwd") } : null });

const view = new Trellis();
let tasks = 0, renders = 0, last;
const cx = {
  spawn(fn) { tasks++; Promise.resolve().then(() => fn(cx)).catch((e) => console.error("task failed", e)).finally(() => tasks--); return { cancel() {} }; },
  timer: { every() { return { cancel() {} }; }, after(ms, fn) { tasks++; setTimeout(() => { tasks--; fn(cx); }, ms); return { cancel() {} }; } },
  notify() {},
};
const settle = async () => { for (let quiet = 0; quiet < 3;) { await new Promise((r) => setTimeout(r, 50)); last = view.render(cx); renders++; quiet = tasks ? 0 : quiet + 1; } };
view.init(state.props, cx);
await settle();
last.find("root").props.size({ width: Number(arg("--width", 900)), height: 800 }, cx);
await settle();
const select = arg("--select");
if (select) {
  const row = view.checkouts?.find((r) => r.rootKey === view.rootKey);
  state.shared["trellis.selectedFile"] = { root: row.workspaceRoot, path: select, on: null };
  await settle();
}
const click = arg("--click");
if (click) {
  const target = last.find(click);
  if (!target?.props.click) throw new Error(`nothing to click: ${click}`);
  target.props.click({}, cx);
  await settle();
}
// --press <kind>: press the centre of the first drawn control of that kind (L band, E arrow).
const press = arg("--press");
if (press) {
  const box = view.current().boxes.find((b) => b.kind === press);
  if (!box) throw new Error(`no drawn ${press}`);
  last.find("diagram").props["press-left"]({ local_position: { x: box.x + box.w / 2, y: box.y + box.h / 2 } }, cx);
  await settle();
}
if (state.execs) console.error(`(${state.execs} requests through exec + curl)`);
console.log(last.text().replace(/\s+/g, " ").slice(0, 4000));
const dir = join(appHome, "mods", "trellis", ".render"), list = () => (existsSync(dir) ? readdirSync(dir) : []);
const rendered = list();
console.log("\nrendered:", rendered, rendered.length ? `${readFileSync(join(dir, rendered[0]), "utf8").length} bytes` : "");
await view.close();
console.log("after close:", list());
