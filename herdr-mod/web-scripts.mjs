// The Trellis browser scripts the mod bundles unchanged (no dependencies; used by the esbuild
// plugin in web-plugin.mjs and by the Node test loader in test/hooks.mjs).
//
// `import make from "trellis-web:<name>"` loads web/<file> and wraps the classic script in
// `export default function (<globals>) { <script>; return <what it publishes>; }`. The
// caller passes the globals the script expects, so nothing touches the runtime's globalThis.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..", "web");

const WEB = {
  // Pele reads only `globalThis.OffscreenCanvas`, `globalThis.document` and `globalThis.fonts`;
  // the parameter shadows the real global, so the caller decides how text is measured.
  pele: { file: "vendor/pele-1.10.0-trellis.2.js", params: "globalThis", result: "pele" },
  "metric-heat": { file: "metric-heat.js", params: "window", result: "window.TrellisMetricHeat" },
  // The pure parts it exports: the summary display (`createDisplay`) and the module helpers.
  architecture: { file: "architecture.js", params: "window, document", result: "window.TrellisArchitecture" },
  // Apps, All apps layout, interface cards and inspector facts (pure).
  "architecture-apps": { file: "architecture-apps.js", params: "window", result: "window.TrellisArchitectureApps" },
  // Only the pure Deviations tree model is used; its DOM view is not.
  "architecture-deviations": { file: "architecture-deviations.js", params: "window, document", result: "window.TrellisDeviations" },
  // What the Pele adapters share (SVG admission, button semantics, the text measurer); they read it from `window`, so load it first.
  "pele-svg": { file: "pele-svg.js", params: "window, document, DOMParser, TextEncoder", result: "window.TrellisPeleSvg" },
  // The arrow routing arithmetic the Architecture adapter reads from `window` (pure); load it first.
  "architecture-routing": { file: "architecture-routing.js", params: "window", result: "window.TrellisArchitectureRouting" },
  "architecture-pele": { file: "architecture-pele.js", params: "window, document, DOMParser, TextEncoder", result: "window.TrellisArchitecturePele" },
  // The shared source-range check the two Sequence scripts read from `window`; load it first.
  "source-witness": { file: "source-witness.js", params: "window", result: "window.TrellisSourceWitness" },
  // The language-neutral Sequence helpers (labels, lanes, display). It draws nothing itself (#1007).
  sequence: { file: "sequence.js", params: "window, document", result: "window.TrellisSequence" },
  "sequence-pele": { file: "sequence-pele.js", params: "window, document, DOMParser, TextEncoder", result: "window.TrellisPeleSequence" },
};

// Data read from the web app's own files at build time, so the mod can't drift from them:
// the `--sm-*` colour tokens of style.css, and the Architecture toolbar's Color by and
// Aggregation choices from index.html.
const DATA = {
  tokens: { file: "style.css", read: (text) => Object.fromEntries([...text.matchAll(/--(sm-[a-z-]+):\s*(#[0-9A-Fa-f]{6})\b/g)].map((m) => [m[1], m[2].toUpperCase()])) },
  controls: { file: "index.html", read: (text) => {
    const options = (id) => {
      const select = new RegExp(`<select id="${id}">([\\s\\S]*?)</select>`).exec(text)?.[1];
      if (!select) throw new Error(`select #${id} not found in index.html`);
      return [...select.matchAll(/<option value="([^"]+)"( selected)?>([^<]+)<\/option>/g)].map((m) => ({ value: m[1], label: m[3], selected: Boolean(m[2]) }));
    };
    return { metrics: options("architecture-color"), aggregates: options("architecture-aggregate") };
  } },
};

/** The ES module source for `trellis-web:<name>`, and the file it comes from. */
export function webModule(name) {
  if (DATA[name]) {
    const file = join(web, DATA[name].file);
    return { contents: `export default ${JSON.stringify(DATA[name].read(readFileSync(file, "utf8")))};\n`, file };
  }
  const spec = WEB[name];
  if (!spec) throw new Error(`unknown trellis-web module ${name}`);
  const file = join(web, spec.file);
  const source = readFileSync(file, "utf8");
  // Pele's own headless text widths (Helvetica metrics, per 1000 em, base-36 pairs for
  // U+0020..U+007E): the mod measures with the same table Pele lays out with.
  const table = name === "pele" ? /var wp="([0-9a-z]{190})"/.exec(source)?.[1] : null;
  if (name === "pele" && !table) throw new Error("Pele width table not found");
  const contents = `export default function (${spec.params}) {\n${source}\n;return ${spec.result};\n}\n` +
    (table ? `export const widthTable = ${JSON.stringify(table)};\n` : "");
  return { contents, file };
}
