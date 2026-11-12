// Bundles the Trellis Herdr mod (#707) into one main.js with esbuild.
//
//   node build.mjs              build into ./dist
//   node build.mjs --install    build, then copy into ~/.config/herdr-app/mods/trellis
//                               ($HERDR_APP_HOME/mods/trellis when that is set), with
//                               setup-viewer.sh, and the trellis layout into layouts/
//   node build.mjs --entry F    bundle another entry (development probes)
//
// The browser's own scripts are bundled unchanged, not copied: each classic script is
// wrapped at build time in a factory that receives the globals it expects (a `window`
// object, and for the Architecture adapter the small DOM in src/dom.js). Nothing is
// added to the mod runtime's real globalThis.
import { build } from "esbuild";
import { webPlugin } from "./web-plugin.mjs";
import { copyFileSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const entryIndex = args.indexOf("--entry");
const entry = entryIndex >= 0 ? resolve(args[entryIndex + 1]) : join(here, "src", "main.js");

const outdir = join(here, "dist");
mkdirSync(outdir, { recursive: true });
const result = await build({
  entryPoints: [entry],
  bundle: true,
  format: "esm",
  target: "es2022",
  outfile: join(outdir, "main.js"),
  // The runtime's own modules.
  external: ["gpui-kit", "gpui-base", "herdr", "fs/promises", "buffer", "zlib"],
  plugins: [webPlugin],
  legalComments: "inline",
  // Pele is already minified; the rest is small. Keep names so runtime stack traces stay readable.
  minifyWhitespace: true,
  minifySyntax: true,
  logLevel: "warning",
  metafile: true,
});
for (const asset of ["mod.json"]) copyFileSync(join(here, asset), join(outdir, asset));
// The web app's brand mark, which the pane's header shows as the browser's does (#886).
copyFileSync(join(here, "..", "web", "brand", "trellis-mark-transparent-v1.png"), join(outdir, "trellis-mark.png"));
const size = statSync(join(outdir, "main.js")).size;
writeFileSync(join(outdir, "meta.json"), JSON.stringify(result.metafile));
console.log(`dist/main.js ${(size / 1024).toFixed(1)} KiB`);

if (args.includes("--install")) {
  const home = process.env.HERDR_APP_HOME || join(homedir(), ".config", "herdr-app");
  const target = join(home, "mods", "trellis");
  mkdirSync(target, { recursive: true });
  // mod.json first: a new main.js reloads every copy, and should see the final mod.json.
  for (const asset of ["mod.json", "trellis-mark.png", "main.js"]) copyFileSync(join(outdir, asset), join(target, asset));
  // A viewing machine streams this from here: `ssh MINI 'cat ~/.config/herdr-app/mods/trellis/setup-viewer.sh' | sh -s MINI`.
  copyFileSync(join(here, "setup-viewer.sh"), join(target, "setup-viewer.sh"));
  console.log(`installed ${target}`);
  // The personal trellis layout (a clone of Herdr's main); a different existing one is kept as .bak.
  const layouts = join(home, "layouts");
  mkdirSync(layouts, { recursive: true });
  const layout = join(layouts, "trellis.json"), source = join(here, "layout", "trellis.json");
  if (existsSync(layout) && readFileSync(layout, "utf8") !== readFileSync(source, "utf8")) {
    copyFileSync(layout, `${layout}.bak`);
    console.log(`kept the previous layout as ${layout}.bak`);
  }
  copyFileSync(source, layout);
  console.log(`installed ${layout}`);
}
