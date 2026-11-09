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
  // Only the pure helpers it exports (moduleKey, commonDirectory, groupModules) are used.
  architecture: { file: "architecture.js", params: "window, document", result: "window.TrellisArchitecture" },
  "architecture-pele": { file: "architecture-pele.js", params: "window, document, DOMParser, TextEncoder", result: "window.TrellisArchitecturePele" },
  // The language-neutral Sequence helpers (labels, lanes, display); its own SVG renderer is unused.
  sequence: { file: "sequence.js", params: "window, document", result: "window.TrellisSequence" },
  "sequence-pele": { file: "sequence-pele.js", params: "window, document, DOMParser, TextEncoder", result: "window.TrellisPeleSequence" },
};

/** The ES module source for `trellis-web:<name>`, and the file it comes from. */
export function webModule(name) {
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
