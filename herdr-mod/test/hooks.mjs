// Node module hooks for the mod's tests: `trellis-web:<name>` loads the wrapped browser script
// (web-scripts.mjs), and the host modules load the stand-ins in test/stubs. No dependencies,
// so `node --test` runs the tests without installing esbuild.
import { webModule } from "../web-scripts.mjs";

const stubs = { "gpui-kit": "stubs/gpui.js", "gpui-base": "stubs/gpui.js", herdr: "stubs/herdr.js" };
export async function resolve(specifier, context, next) {
  if (specifier.startsWith("trellis-web:")) return { url: specifier, shortCircuit: true };
  if (stubs[specifier]) return { url: new URL(stubs[specifier], import.meta.url).href, shortCircuit: true };
  return next(specifier, context);
}
export async function load(url, context, next) {
  if (url.startsWith("trellis-web:")) return { format: "module", source: webModule(url.slice("trellis-web:".length)).contents, shortCircuit: true };
  return next(url, context);
}
