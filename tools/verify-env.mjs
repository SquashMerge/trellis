#!/usr/bin/env node
// Provision verify-only Node dependencies and the pinned Chromium outside the checkout.
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { accessSync, chmodSync, constants, lstatSync, mkdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const root = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), ".."));
const digest = (value) => createHash("sha256").update(value).digest("hex");

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

function ensureLockedPackage(cache, dir, required, env) {
  const packageRoot = join(root, dir);
  const stamp = join(cache, `.verify-${dir.replaceAll("/", "-")}.sha256`);
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
  writeFileSync(stamp, "");
  run("npm", ["ci", "--prefix", dir, "--ignore-scripts"], env);
  if (!installedMatchesLock()) throw new Error(`Locked ${dir} dependencies are incomplete after npm ci`);
  writeFileSync(stamp, `${fingerprint}\n`);
}

function main() {
  if (realpathSync(process.cwd()) !== root) throw new Error("Run ./tools/verify from the repository root");
  if (process.env.TRELLIS_RUN_CACHE === "") throw new Error("TRELLIS_RUN_CACHE must not be empty");
  if (process.env.PLAYWRIGHT_BROWSERS_PATH === "") throw new Error("PLAYWRIGHT_BROWSERS_PATH must not be empty");
  const defaultParent = join(tmpdir(), "trellis-verify");
  const defaultCache = join(defaultParent, digest(`${process.getuid?.() ?? "user"}:${root}`).slice(0, 24));
  if (process.env.TRELLIS_RUN_CACHE === undefined) externalDirectory(defaultParent, "default cache parent", true);
  const cache = externalDirectory(process.env.TRELLIS_RUN_CACHE ?? defaultCache, "TRELLIS_RUN_CACHE", process.env.TRELLIS_RUN_CACHE === undefined);
  const browsers = externalDirectory(process.env.PLAYWRIGHT_BROWSERS_PATH ?? join(cache, "playwright-browsers"), "PLAYWRIGHT_BROWSERS_PATH", process.env.PLAYWRIGHT_BROWSERS_PATH === undefined);
  const npmCache = externalDirectory(join(cache, "npm-cache"), "npm cache", true);
  process.env.PLAYWRIGHT_BROWSERS_PATH = browsers;
  const env = { ...process.env, npm_config_cache: npmCache, PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1" };
  ensureLockedPackage(cache, "tests/mcp", ["@modelcontextprotocol/sdk", "ajv"], env);
  ensureLockedPackage(cache, "tests/browser", ["playwright", "playwright-core"], env);
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

try { main(); } catch (error) { console.error(`verify environment: ${error.message}`); process.exitCode = 1; }
