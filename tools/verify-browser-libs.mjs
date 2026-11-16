#!/usr/bin/env node
// Report whether the pinned Chromium that verify-env.mjs downloaded can load
// and lay text out as the browser suite expects on this Linux host without
// installing system packages. tools/verify runs `playwright install-deps`
// (apt) only when this exits non-zero, so CI does not depend on the apt
// mirrors when the runner image already has what that install provides.
//
// Exit 0: every shared library of both Chromium builds resolves (ldd) and the
// font the layout tests measure text in is installed (fc-list).
// Exit 1: a library or the font is missing, or a check itself could not run;
// either way verify falls back to installing the system packages.
import { spawnSync } from "node:child_process";
import { readdirSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), ".."));

// Unresolved libraries of one file, or null when ldd cannot inspect it.
function missingLibraries(file, directory) {
  const libraryPath = [process.env.LD_LIBRARY_PATH, directory].filter(Boolean).join(":");
  const result = spawnSync("ldd", [file], { cwd: directory, encoding: "utf8", env: { ...process.env, LD_LIBRARY_PATH: libraryPath } });
  if (result.error || result.status !== 0) return null;
  return result.stdout.split("\n").map((line) => line.trim())
    .filter((line) => line.includes("=>") && line.endsWith("not found"))
    .map((line) => line.split("=>")[0].trim());
}

// `playwright install-deps` also installs fonts. The browser suite's layout
// tests measure text in the host's default sans-serif, and their bounds hold
// for Liberation Sans (fonts-liberation); a runner image with the libraries
// but without that font lays diagrams out wider and fails them (#1010).
const FONT = "Liberation Sans";

// Whether fontconfig lists the font, or null when fc-list cannot run.
function fontInstalled() {
  const result = spawnSync("fc-list", [":", "family"], { encoding: "utf8" });
  if (result.error || result.status !== 0) return null;
  return result.stdout.split("\n").some((line) => line.split(",").some((family) => family.trim() === FONT));
}

function main() {
  // Pinned by tests/browser/package-lock.json. `chromium.launch()` runs the
  // headless shell for headless tests; verify-env.mjs installs both builds.
  const requireBrowser = createRequire(join(root, "tests/browser/package.json"));
  const { registry } = requireBrowser("playwright-core/lib/coreBundle").registry;
  const missing = new Set();
  for (const name of ["chromium", "chromium-headless-shell"]) {
    const executable = registry.findExecutable(name)?.executablePath();
    if (!executable) throw new Error(`Playwright has no ${name} executable on this host`);
    const directory = dirname(executable);
    const own = missingLibraries(executable, directory);
    if (own === null) throw new Error(`ldd could not inspect ${executable}`);
    own.forEach((library) => missing.add(library));
    // Chromium loads the libraries bundled beside it (GL, Vulkan) at run time.
    for (const entry of readdirSync(directory)) {
      if (!/\.so(\.|$)/.test(entry)) continue;
      const bundled = missingLibraries(join(directory, entry), directory);
      if (bundled === null) throw new Error(`ldd could not inspect ${join(directory, entry)}`);
      bundled.forEach((library) => missing.add(library));
    }
    console.log(`verify browser libraries: checked ${executable}`);
  }
  if (missing.size) {
    console.log(`verify browser libraries: missing ${[...missing].sort().join(", ")}`);
    return 1;
  }
  console.log("verify browser libraries: every shared library resolves");
  const font = fontInstalled();
  if (font === null) throw new Error("fc-list could not list the installed fonts");
  if (!font) {
    // Best effort, for the log: what the layout tests would have measured in.
    const fallback = spawnSync("fc-match", ["sans-serif", "family"], { encoding: "utf8" });
    const resolved = fallback.status === 0 && fallback.stdout.trim() ? fallback.stdout.trim() : "unknown";
    console.log(`verify browser libraries: missing font ${FONT} (sans-serif resolves to ${resolved})`);
    return 1;
  }
  console.log(`verify browser libraries: font ${FONT} is installed`);
  return 0;
}

try {
  process.exitCode = main();
} catch (error) {
  console.log(`verify browser libraries: cannot check (${error.message})`);
  process.exitCode = 1;
}
