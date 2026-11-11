#!/usr/bin/env node
// Report whether the pinned Chromium that verify-env.mjs downloaded can load
// on this Linux host without installing system packages. tools/verify runs
// `playwright install-deps` (apt) only when this exits non-zero, so CI does not
// depend on the apt mirrors when the runner image already has the libraries.
//
// Exit 0: every shared library of both Chromium builds resolves (ldd).
// Exit 1: a library is missing, or the check itself could not run; either way
// verify falls back to installing the system packages.
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
      (missingLibraries(join(directory, entry), directory) ?? []).forEach((library) => missing.add(library));
    }
    console.log(`verify browser libraries: checked ${executable}`);
  }
  if (missing.size) {
    console.log(`verify browser libraries: missing ${[...missing].sort().join(", ")}`);
    return 1;
  }
  console.log("verify browser libraries: every shared library resolves");
  return 0;
}

try {
  process.exitCode = main();
} catch (error) {
  console.log(`verify browser libraries: cannot check (${error.message})`);
  process.exitCode = 1;
}
