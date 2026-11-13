#!/usr/bin/env node
// Provision verify-only Node dependencies and the pinned Chromium outside the checkout.
//
// Cache layout (#1014). Every checkout shares one cache, by default
// `$TMPDIR/trellis-verify/`, holding `playwright-browsers/` and `npm-cache/`.
// Sharing is safe for the two or three verifies that may provision at once:
// - `playwright install` holds `<browsers>/__dirlock` for its whole run, skips a
//   build whose INSTALLATION_COMPLETE marker exists, and writes that marker only
//   after extraction, so no process is handed a partial browser. Playwright names
//   each build's directory by revision, so different pins sit side by side, and
//   under the same lock it removes builds that no registered, still-installed
//   playwright-core refers to (a deleted checkout stops counting).
// - npm's cache is content-addressed and written atomically.
// The only per-checkout state is the marker that says `npm ci` matched the lock
// file; it lives in that checkout's own `node_modules`.
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { accessSync, chmodSync, constants, lstatSync, mkdirSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const root = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), ".."));
const digest = (value) => createHash("sha256").update(value).digest("hex");

// Before #1014 each checkout had its own `<parent>/<24 hex>/` with a full browser copy.
const LEGACY_ENTRY = /^[0-9a-f]{24}$/;
export const LEGACY_IDLE_MS = 24 * 60 * 60 * 1000;

export function cacheLayout(checkout, env, temp, uid) {
  const parent = join(temp, "trellis-verify");
  const cache = env.TRELLIS_RUN_CACHE ?? parent;
  return {
    parent,
    cache,
    browsers: env.PLAYWRIGHT_BROWSERS_PATH ?? join(cache, "playwright-browsers"),
    npmCache: join(cache, "npm-cache"),
    legacyEntry: digest(`${uid}:${checkout}`).slice(0, 24),
  };
}

// Remove per-checkout directories the old layout left under the default parent:
// this checkout's own at once, since it no longer uses it, and any other once
// nothing in it has changed for a day. Another checkout still on the old layout
// touches its entry on every verify (the installer's lock updates the browser
// directory), so an idle entry is not in use by a running verify, and an old
// verify that finds its entry gone simply provisions again. Best effort: a
// failure here must not fail verify. Nothing outside `parent` is touched.
export function pruneLegacyEntries(parent, ownEntry, now = Date.now()) {
  let names;
  try { names = readdirSync(parent); } catch { return []; }
  const removed = [];
  for (const name of names) {
    if (!LEGACY_ENTRY.test(name)) continue;
    const entry = join(parent, name);
    try {
      if (!lstatSync(entry).isDirectory()) continue;
      if (name !== ownEntry) {
        const paths = [entry, ...readdirSync(entry).map((child) => join(entry, child))];
        if (paths.some((path) => now - lstatSync(path).mtimeMs < LEGACY_IDLE_MS)) continue;
      }
      rmSync(entry, { recursive: true, force: true });
      removed.push(name);
    } catch {}
  }
  return removed;
}

function physicalCandidate(path) {
  let parent = path;
  const missing = [];
  while (!lstatSync(parent, { throwIfNoEntry: false })) {
    const next = dirname(parent);
    if (next === parent) throw new Error(`No existing ancestor for ${path}`);
    missing.unshift(basename(parent));
    parent = next;
  }
  return resolve(realpathSync(parent), ...missing);
}

function externalDirectory(path, label, privateDirectory = false) {
  if (!isAbsolute(path)) throw new Error(`${label} must be an absolute path outside the checkout`);
  const candidate = physicalCandidate(path);
  const rel = relative(root, candidate);
  if (rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel))) {
    throw new Error(`${label} must be outside the checkout`);
  }
  mkdirSync(path, { recursive: true, mode: 0o700 });
  if (realpathSync(path) !== candidate || !lstatSync(path).isDirectory()) throw new Error(`${label} changed during setup`);
  const info = statSync(path);
  if (process.getuid && info.uid !== process.getuid()) throw new Error(`${label} must be owned by this user`);
  if (privateDirectory) chmodSync(path, 0o700);
  else if (info.mode & 0o022) throw new Error(`${label} must not be group- or world-writable`);
  accessSync(path, constants.W_OK);
  return candidate;
}

function run(command, args, env) {
  const result = spawnSync(command, args, { cwd: root, env, stdio: ["inherit", 2, 2] });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} failed (${result.status ?? result.signal})`);
}

function ensureLockedPackage(dir, required, env) {
  const packageRoot = join(root, dir);
  // `npm ci` replaces node_modules, so the marker cannot outlive the install it describes.
  const stamp = join(packageRoot, "node_modules", ".trellis-verify.sha256");
  const packageBytes = readFileSync(join(packageRoot, "package.json"));
  const lockBytes = readFileSync(join(packageRoot, "package-lock.json"));
  const fingerprint = digest(Buffer.concat([packageBytes, lockBytes]));
  const packages = JSON.parse(lockBytes).packages;
  const installedMatchesLock = () => required.every((name) => {
    const expected = packages?.[`node_modules/${name}`]?.version;
    if (!expected) throw new Error(`Missing locked ${dir} package: ${name}`);
    try {
      return JSON.parse(readFileSync(join(packageRoot, "node_modules", name, "package.json"))).version === expected;
    } catch { return false; }
  });
  let recorded;
  try { recorded = readFileSync(stamp, "utf8").trim(); } catch {}
  if (recorded === fingerprint && installedMatchesLock()) return;
  // A failed npm ci must never leave an old success marker behind.
  rmSync(stamp, { force: true });
  run("npm", ["ci", "--prefix", dir, "--ignore-scripts"], env);
  if (!installedMatchesLock()) throw new Error(`Locked ${dir} dependencies are incomplete after npm ci`);
  writeFileSync(stamp, `${fingerprint}\n`);
}

function main() {
  if (realpathSync(process.cwd()) !== root) throw new Error("Run ./tools/verify from the repository root");
  if (process.env.TRELLIS_RUN_CACHE === "") throw new Error("TRELLIS_RUN_CACHE must not be empty");
  if (process.env.PLAYWRIGHT_BROWSERS_PATH === "") throw new Error("PLAYWRIGHT_BROWSERS_PATH must not be empty");
  const layout = cacheLayout(root, process.env, tmpdir(), process.getuid?.() ?? "user");
  const cache = externalDirectory(layout.cache, "TRELLIS_RUN_CACHE", process.env.TRELLIS_RUN_CACHE === undefined);
  const browsers = externalDirectory(layout.browsers, "PLAYWRIGHT_BROWSERS_PATH", process.env.PLAYWRIGHT_BROWSERS_PATH === undefined);
  const npmCache = externalDirectory(layout.npmCache, "npm cache", true);
  if (cache === physicalCandidate(layout.parent)) pruneLegacyEntries(cache, layout.legacyEntry);
  process.env.PLAYWRIGHT_BROWSERS_PATH = browsers;
  const env = { ...process.env, npm_config_cache: npmCache, PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1" };
  ensureLockedPackage("tests/mcp", ["@modelcontextprotocol/sdk", "ajv"], env);
  ensureLockedPackage("tests/browser", ["playwright", "playwright-core"], env);
  const cli = join(root, "tests/browser/node_modules/playwright/cli.js");
  const requireBrowser = createRequire(join(root, "tests/browser/package.json"));
  const executable = requireBrowser("playwright").chromium.executablePath();
  // Playwright also needs its headless-shell bundle; its installer checks both
  // installation markers and repairs a missing component without downloading healthy ones.
  run(process.execPath, [cli, "install", "chromium"], env);
  const actual = realpathSync(executable);
  const rel = relative(browsers, actual);
  if (rel === "" || rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel)) {
    throw new Error(`Chromium must be inside PLAYWRIGHT_BROWSERS_PATH: ${actual}`);
  }
  accessSync(actual, constants.X_OK);
  process.stdout.write(`${browsers}\n`);
}

// Tests import the layout functions; only a direct run provisions.
if (process.argv[1] && realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(); } catch (error) { console.error(`verify environment: ${error.message}`); process.exitCode = 1; }
}
