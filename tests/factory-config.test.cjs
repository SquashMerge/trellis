const assert = require("node:assert/strict");
const { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { delimiter, dirname, join, resolve: resolvePath } = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");

const ROOT = join(__dirname, "..");
const CONFIG_PATH = join(ROOT, ".factory.json");
const REQUIRED = ["resolve", "verify"];
const ALLOWED = new Set([
  ...REQUIRED,
  "publish",
  "pr_draft",
  "verify_timeout_ms",
  "bootstrap",
  "bootstrap_timeout_ms",
  "max_retries",
]);

function loadConfig() {
  return JSON.parse(readFileSync(CONFIG_PATH, "utf8"));
}

function makeFakeGh() {
  const dir = mkdtempSync(join(tmpdir(), "trellis-factory-gh-"));
  const command = join(dir, "gh");
  writeFileSync(
    command,
    `#!/bin/sh
printf '%s\n' "$@" > "$FAKE_GH_LOG"
if [ "\${FAKE_GH_EXIT:-0}" -ne 0 ]; then exit "$FAKE_GH_EXIT"; fi
printf '%s\n' '{"run_id":"123","number":123,"title":"Story","body":"Body","url":"https://github.com/SquashMerge/trellis/issues/123","state":"OPEN","labels":[],"assignees":[]}'
`,
  );
  chmodSync(command, 0o755);
  return dir;
}

function resolve(config, fakeDir, input, extraEnv = {}) {
  const log = join(fakeDir, "args.log");
  const result = spawnSync("/bin/sh", ["-c", config.resolve], {
    cwd: ROOT,
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${fakeDir}${delimiter}${process.env.PATH ?? ""}`,
      FACTORY_INPUT: input,
      FAKE_GH_LOG: log,
      ...extraEnv,
    },
  });
  let args = null;
  try {
    args = readFileSync(log, "utf8").trimEnd().split("\n");
  } catch {}
  return { ...result, args };
}

test("factory config uses the closed supported schema", () => {
  const config = loadConfig();
  assert.deepEqual(Object.keys(config).filter((key) => !ALLOWED.has(key)), []);
  for (const key of REQUIRED) {
    assert.equal(typeof config[key], "string");
    assert.notEqual(config[key].trim(), "");
  }
  assert.equal(Object.hasOwn(config, "publish"), false, "use the default PR publisher, not a push-only override");
  assert.equal(typeof config.pr_draft, "boolean");
  assert.equal(Number.isSafeInteger(config.verify_timeout_ms) && config.verify_timeout_ms > 0, true);
  assert.equal(typeof config.bootstrap, "string");
  assert.notEqual(config.bootstrap.trim(), "");
  assert.equal(Number.isSafeInteger(config.bootstrap_timeout_ms) && config.bootstrap_timeout_ms > 0, true);
  assert.equal(config.max_retries, 5);
});

test("verify setup refuses a cache inside the checkout before installing packages", () => {
  const forbidden = join(ROOT, ".verify-cache-probe");
  assert.equal(existsSync(forbidden), false);
  const result = spawnSync(process.execPath, [join(ROOT, "tools/verify-env.mjs")], {
    cwd: ROOT,
    encoding: "utf8",
    env: { ...process.env, TRELLIS_RUN_CACHE: forbidden },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /TRELLIS_RUN_CACHE must be outside the checkout/);
  assert.equal(existsSync(forbidden), false);
});

test("verify setup rejects symlinked and writable external caches", () => {
  const temp = mkdtempSync(join(tmpdir(), "trellis-verify-policy-"));
  const link = join(temp, "checkout-link");
  symlinkSync(ROOT, link, "dir");
  try {
    for (const cache of [link, temp]) {
      if (cache === temp) chmodSync(temp, 0o777);
      const result = spawnSync(process.execPath, [join(ROOT, "tools/verify-env.mjs")], {
        cwd: ROOT,
        encoding: "utf8",
        env: { ...process.env, TRELLIS_RUN_CACHE: cache },
      });
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, cache === link ? /outside the checkout/ : /group- or world-writable/);
    }
  } finally {
    chmodSync(temp, 0o700);
    rmSync(temp, { recursive: true, force: true });
  }
});

test("resolver accepts only canonical Trellis issue references", () => {
  const config = loadConfig();
  const fakeDir = makeFakeGh();
  try {
    const accepted = [
      "123",
      "#123",
      "https://github.com/SquashMerge/trellis/issues/123",
    ];
    for (const input of accepted) {
      const result = resolve(config, fakeDir, input);
      assert.equal(result.status, 0, input);
      assert.equal(JSON.parse(result.stdout).run_id, "123", input);
      assert.deepEqual(result.args, [
        "issue", "view", "123", "--repo", "SquashMerge/trellis", "--json",
        "number,title,body,url,state,labels,assignees", "--jq",
        "{run_id:(.number|tostring)} + .",
      ], input);
    }

    const rejected = [
      "", "0", "01", "+123", "-123", " 123", "123 ", "#abc", "123\n456",
      "https://github.com/other/trellis/issues/123",
      "https://github.com/SquashMerge/trellis/issues/123/extra",
      "https://github.com/SquashMerge/trellis/issues/123?state=open",
      "not an issue",
    ];
    for (const input of rejected) {
      rmSync(join(fakeDir, "args.log"), { force: true });
      const result = resolve(config, fakeDir, input);
      assert.equal(result.status, 0, input);
      assert.equal(result.stdout, "", input);
      assert.equal(result.args, null, input);
    }
  } finally {
    rmSync(fakeDir, { recursive: true, force: true });
  }
});

test("recognized issue lookup failures remain failures", () => {
  const config = loadConfig();
  const fakeDir = makeFakeGh();
  try {
    const result = resolve(config, fakeDir, "123", { FAKE_GH_EXIT: "23" });
    assert.equal(result.status, 23);
    assert.deepEqual(result.args?.slice(0, 3), ["issue", "view", "123"]);
  } finally {
    rmSync(fakeDir, { recursive: true, force: true });
  }
});

// verify skips apt only when this helper proves Chromium's libraries resolve
// and the font the layout tests measure in is installed; any library ldd
// cannot inspect, or a missing or uncheckable font, must send verify back to apt.
const BROWSER_PACKAGES = join(ROOT, "tests/browser/node_modules/playwright-core");
test("browser library check fails when ldd cannot inspect a bundled library or the layout font is absent", { skip: !existsSync(BROWSER_PACKAGES) && "tests/browser packages are not installed" }, () => {
  const fakeDir = mkdtempSync(join(tmpdir(), "trellis-browser-libs-"));
  try {
    const browsers = join(fakeDir, "browsers");
    const env = { ...process.env, PLAYWRIGHT_BROWSERS_PATH: browsers };
    const located = spawnSync(process.execPath, ["-e", `
      const { registry } = require("playwright-core/lib/coreBundle").registry;
      for (const name of ["chromium", "chromium-headless-shell"]) console.log(registry.findExecutable(name).executablePath());
    `], { cwd: join(ROOT, "tests/browser"), env, encoding: "utf8" });
    assert.equal(located.status, 0, located.stderr);
    for (const executable of located.stdout.trim().split("\n")) {
      mkdirSync(dirname(executable), { recursive: true });
      writeFileSync(executable, "");
      writeFileSync(join(dirname(executable), "libbundled.so"), "");
    }
    // Only the fake ldd is on PATH. In "spawn" mode it removes itself after
    // inspecting the first executable, so the bundled library's spawn fails.
    const ldd = join(fakeDir, "bin", "ldd");
    const install = () => {
      mkdirSync(dirname(ldd), { recursive: true });
      writeFileSync(ldd, `#!/bin/sh
case "$1" in *.so) if [ "$FAKE_LDD" = nonzero ]; then echo "not a dynamic executable" >&2; exit 1; fi ;; esac
printf '\\tlibc.so.6 => /lib/libc.so.6 (0x0)\\n'
if [ "$FAKE_LDD" = spawn ]; then /bin/rm -f "$0"; fi
`);
      chmodSync(ldd, 0o755);
    };
    // The fake fontconfig lists one family per line, as `fc-list : family` does.
    const fcList = join(fakeDir, "bin", "fc-list");
    const installFonts = () => {
      writeFileSync(fcList, `#!/bin/sh
if [ "$FAKE_FONTS" = fail ]; then exit 1; fi
echo "DejaVu Sans Mono"
if [ "$FAKE_FONTS" != missing ]; then echo "Liberation Sans Narrow"; echo "Liberation Sans,Liberation Sans Regular"; fi
`);
      chmodSync(fcList, 0o755);
      writeFileSync(join(fakeDir, "bin", "fc-match"), "#!/bin/sh\necho 'DejaVu Sans Mono'\n");
      chmodSync(join(fakeDir, "bin", "fc-match"), 0o755);
    };
    const check = (mode, fonts = "ok") => {
      install();
      if (fonts === "absent") rmSync(fcList, { force: true });
      else installFonts();
      return spawnSync(process.execPath, ["tools/verify-browser-libs.mjs"], {
        cwd: ROOT, encoding: "utf8", env: { ...env, PATH: dirname(ldd), FAKE_LDD: mode, FAKE_FONTS: fonts },
      });
    };
    const resolved = check("ok");
    assert.equal(resolved.status, 0, resolved.stdout + resolved.stderr);
    assert.match(resolved.stdout, /every shared library resolves/);
    assert.match(resolved.stdout, /^verify browser libraries: font Liberation Sans is installed$/m);
    // Libraries present but the font is not (#1010): verify must take the apt route.
    const noFont = check("ok", "missing");
    assert.equal(noFont.status, 1, noFont.stdout + noFont.stderr);
    assert.match(noFont.stdout, /every shared library resolves/);
    assert.match(noFont.stdout, /^verify browser libraries: missing font Liberation Sans \(sans-serif resolves to DejaVu Sans Mono\)$/m);
    for (const fonts of ["fail", "absent"]) {
      const unchecked = check("ok", fonts);
      assert.equal(unchecked.status, 1, `${fonts}: ${unchecked.stdout}${unchecked.stderr}`);
      assert.match(unchecked.stdout, /cannot check \(fc-list could not list the installed fonts\)/, fonts);
    }
    for (const mode of ["nonzero", "spawn"]) {
      const failed = check(mode);
      assert.equal(failed.status, 1, `${mode}: ${failed.stdout}${failed.stderr}`);
      assert.match(failed.stdout, /cannot check \(ldd could not inspect .*libbundled\.so\)/, mode);
      assert.doesNotMatch(failed.stdout, /every shared library resolves/, mode);
    }
  } finally {
    rmSync(fakeDir, { recursive: true, force: true });
  }
});

test("verify is one executable command and retains all checks", () => {
  const config = loadConfig();
  assert.equal(config.verify, "./tools/verify");
  assert.notEqual(statSync(join(ROOT, "tools/verify")).mode & 0o111, 0);
  const fakeDir = mkdtempSync(join(tmpdir(), "trellis-factory-verify-"));
  const log = join(fakeDir, "verify.log");
  const fakeTool = `#!/bin/sh
line="\${0##*/}"
for arg in "$@"; do line="$line$(printf '\\t')$arg"; done
# One append per call keeps lines whole when lanes run concurrently.
printf '%s\\n' "$line" >> "$FAKE_VERIFY_LOG"
if [ "$1" = tools/verify-env.mjs ]; then
  if [ "\${FAKE_VERIFY_FAIL_SETUP:-0}" = 1 ]; then exit 31; fi
  printf '%s\n' "$FAKE_BROWSER_ROOT"
  exit 0
fi
# verify hands the browser suite the binary it built (#192).
for arg in "$@"; do
  if [ "$arg" = tests/browser/sequence-harness.test.cjs ] && [ -z "\${TRELLIS_SUITE_BINARY:-}" ]; then exit 37; fi
  if [ "$arg" = tests/browser/navigation-resolved.test.cjs ] && [ -z "\${TRELLIS_NAVIGATION_FIXTURE_BINARY:-}" ]; then exit 38; fi
done
if [ "$1" = tools/verify-browser-libs.mjs ]; then
  if [ "\${FAKE_VERIFY_MISSING_LIBS:-0}" = 1 ]; then echo "verify browser libraries: missing libfake.so.1"; exit 1; fi
  echo "verify browser libraries: every shared library resolves"
  exit 0
fi
if [ "\${FAKE_VERIFY_FAIL_DEPS:-0}" = 1 ] && [ "$2" = install-deps ]; then echo "fake apt failure"; exit 41; fi
if [ -n "\${FAKE_VERIFY_HANG_DEPS:-}" ] && [ "$2" = install-deps ]; then echo $$ > "$FAKE_VERIFY_HANG_DEPS"; exec sleep 600; fi
if [ "\${FAKE_VERIFY_NO_NEXTEST:-0}" = 1 ] && [ "$1" = nextest ]; then exit 101; fi
if [ "$1" = nextest ] && [ "$2" = --version ]; then echo "cargo-nextest \${FAKE_VERIFY_NEXTEST_VERSION:-0.9.150}"; exit 0; fi
if [ "\${FAKE_VERIFY_FAIL_CLIPPY:-0}" = 1 ] && [ "$1" = clippy ]; then
  # Fail only once a hanging install is running, so the test is deterministic.
  if [ -n "\${FAKE_VERIFY_HANG_DEPS:-}" ]; then while [ ! -s "$FAKE_VERIFY_HANG_DEPS" ]; do sleep 0.1; done; fi
  exit 23
fi
if [ "\${FAKE_VERIFY_FAIL_SEMANTIC:-0}" = 1 ] && [ "$1" = tools/semantic-contract/test/run.mjs ]; then exit 29; fi
`;
  try {
    // This test checks orchestration only; the real suites run once in verify.
    for (const name of ["cargo", "node", "python3", "cp"]) {
      const command = join(fakeDir, name);
      writeFileSync(command, fakeTool);
      chmodSync(command, 0o755);
    }
    function runVerify(extraEnv = {}, timeout = undefined, args = []) {
      const result = spawnSync(config.verify, args, {
        cwd: ROOT,
        timeout,
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${fakeDir}${delimiter}${process.env.PATH ?? ""}`,
          FAKE_VERIFY_LOG: log,
          FAKE_BROWSER_ROOT: join(fakeDir, "browser-cache"),
          // Linux CI sets this for the real verify; the fake runs opt in per case.
          TRELLIS_BROWSER_SYSTEM_DEPS: "",
          ...extraEnv,
        },
      });
      const text = existsSync(log) ? readFileSync(log, "utf8").trimEnd() : "";
      const commands = text === "" ? [] : text.split("\n").map((line) => line.split("\t"));
      return { result, commands, output: result.stdout + result.stderr };
    }
    // Lanes run concurrently; commands within a lane keep this order.
    const lanes = {
      setup: [["node", "tools/verify-env.mjs"]],
      rust: [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
        ["cargo", "clippy", "--locked", "--all-targets", "--features", "test-support", "--", "-D", "warnings"],
        ["cargo", "build", "--locked", "--features", "test-support", "--bin", "trellis"],
        ["cp", join(resolvePath(ROOT, process.env.CARGO_TARGET_DIR || "target"), "debug", "trellis"), join(resolvePath(ROOT, process.env.CARGO_TARGET_DIR || "target"), "debug", "trellis-navigation-fixture")],
        ["cargo", "build", "--locked", "--bin", "trellis"],
        ["python3", "tools/verify-40-query-performance.py", "--smoke", "--binary", join(resolvePath(ROOT, process.env.CARGO_TARGET_DIR || "target"), "debug", "trellis")],
        ["node", "--test", "--test-concurrency=1", "tests/browser/sequence-harness.test.cjs", "tests/browser/pele-block-labels.test.cjs", "tests/browser/sequence-pele.test.cjs", "tests/browser/sequence-readable.test.cjs", "tests/browser/sequence-interactions.test.cjs", "tests/browser/sequence-hops.test.cjs", "tests/browser/sequence-hops-app.test.cjs", "tests/browser/sequence-callbacks.test.cjs", "tests/browser/sequence-callbacks-app.test.cjs", "tests/browser/sequence.test.cjs", "tests/browser/sequence-cost.test.cjs", "tests/browser/brand.test.cjs", "tests/browser/connect.test.cjs", "tests/browser/stall-recovery.test.cjs", "tests/browser/viewer-shell.test.cjs", "tests/browser/files.test.cjs", "tests/browser/architecture-pele.test.cjs", "tests/browser/architecture.test.cjs", "tests/browser/architecture-apps.test.cjs", "tests/browser/architecture-routing.test.cjs", "tests/browser/app-switcher.test.cjs", "tests/browser/treemap.test.cjs", "tests/browser/classes-pele.test.cjs", "tests/browser/classes-app.test.cjs", "tests/browser/classes-languages.test.cjs", "tests/browser/navigation-resolved.test.cjs"],
      ],
      tests: [
        ["cargo", "nextest", "--version"],
        ["cargo", "nextest", "run", "--locked", "--all-targets", "--no-fail-fast", "--features", "test-support"],
      ],
      semantic: [["node", "tools/semantic-contract/test/run.mjs"]],
      cohorts: [["node", "--test", "tools/synthetic-cohorts/test/cohorts.test.mjs"]],
      checks: [
        ["python3", "tools/flaky-patterns-test.py"],
        ["python3", "tools/flaky-patterns.py"],
        ["node", "--test", "tests/mcp-catalog-sdk.test.mjs", "tests/mcp-smoke.test.mjs"],
        ["python3", "tools/stage3-watcher-runner-test.py"],
        ["python3", "tools/verify-67-mixed-performance-test.py"],
        ["python3", "tools/verify-40-query-performance-test.py"],
        ["python3", "tools/verify-61-combined-gate-test.py"],
        ["python3", "-m", "unittest", "discover", "-s", "tools/semantic-contract/test", "-p", "test_*.py"],
        ["python3", "-m", "unittest", "discover", "-s", "tools/release", "-p", "test_*.py"],
        ["node", "--test", "tests/factory-config.test.cjs"],
        ["node", "docs/semantic-evidence/check-policy.mjs", "--self-test"],
        ["node", "--check", "web/app.js"],
        ["node", "--check", "web/index-pin.js"],
        ["node", "--check", "web/source-witness.js"],
        ["node", "--check", "web/context-menu.js"],
        ["node", "--check", "web/pele-svg.js"],
        ["node", "--check", "web/architecture-apps.js"],
        ["node", "--import", "./herdr-mod/test/register.mjs", "--test", "herdr-mod/test/mod.test.mjs", "herdr-mod/test/view.test.mjs", "herdr-mod/test/parity.test.mjs", "herdr-mod/test/setup.test.mjs"],
        ["node", "--test", "tests/browse-ui.test.cjs", "tests/sequence-ui.test.cjs", "tests/token-ui.test.cjs", "tests/external-source-ui.test.cjs", "tests/dependency-ui.test.cjs", "tests/shell-ui.test.cjs", "tests/classes-ui.test.cjs", "tests/navigation-ui.test.cjs", "tests/metric-heat.test.cjs", "tests/viewer-metrics-ui.test.cjs", "tests/files-ui.test.cjs", "tests/architecture-ui.test.cjs", "tests/architecture-pele-ui.test.cjs", "tests/treemap-ui.test.cjs", "tests/treemap-pele.test.cjs", "tests/architecture-apps-ui.test.cjs", "tests/app-switcher-ui.test.cjs"],
      ],
    };
    function assertRan(commands, expected) {
      const key = (command) => JSON.stringify(command);
      assert.deepEqual(commands.map(key).sort(), Object.values(expected).flat().map(key).sort());
      assert.deepEqual(commands[0], lanes.setup[0], "bootstrap must finish before parallel lanes");
      for (const lane of Object.values(expected)) {
        const own = new Set(lane.map(key));
        assert.deepEqual(commands.filter((command) => own.has(key(command))), lane);
      }
    }
    const setupFailure = runVerify({ FAKE_VERIFY_FAIL_SETUP: "1" });
    assert.equal(setupFailure.result.status, 31);
    assert.deepEqual(setupFailure.commands, lanes.setup, "failed setup must not start other lanes");
    rmSync(log);
    const success = runVerify();
    assert.equal(success.result.status, 0, success.output);
    assertRan(success.commands, lanes);
    rmSync(log);
    // With TRELLIS_BROWSER_SYSTEM_DEPS set, the system packages are checked
    // and, only if a library or the layout font is missing, installed in the background; the
    // browser suite runs only after that succeeds (#244).
    const libs = ["node", "tools/verify-browser-libs.mjs"];
    const deps = ["node", "tests/browser/node_modules/playwright/cli.js", "install-deps", "chromium"];
    const present = runVerify({ TRELLIS_BROWSER_SYSTEM_DEPS: "1" });
    assert.equal(present.result.status, 0, present.output);
    assertRan(present.commands, { ...lanes, deps: [libs] });
    assert.match(present.result.stdout, /^verify: Chromium's shared libraries all resolve and its layout font is installed; skipped the system package install \(no apt\)/m);
    rmSync(log);
    const withDeps = runVerify({ TRELLIS_BROWSER_SYSTEM_DEPS: "1", FAKE_VERIFY_MISSING_LIBS: "1" });
    assert.equal(withDeps.result.status, 0, withDeps.output);
    assertRan(withDeps.commands, { ...lanes, deps: [libs, deps] });
    assert.match(withDeps.result.stdout, /missing libfake\.so\.1/);
    assert.match(withDeps.result.stdout, /^verify: Chromium system package install \(apt\) finished in \d+ s \(exit 0\)/m);
    rmSync(log);
    const depsFailure = runVerify({ TRELLIS_BROWSER_SYSTEM_DEPS: "1", FAKE_VERIFY_MISSING_LIBS: "1", FAKE_VERIFY_FAIL_DEPS: "1" });
    assert.equal(depsFailure.result.status, 1);
    assertRan(depsFailure.commands, { ...lanes, deps: [libs, deps], rust: lanes.rust.slice(0, -1) });
    assert.match(depsFailure.result.stdout, /fake apt failure/);
    assert.match(depsFailure.result.stdout, /Chromium system package install failed \(exit 41\)/);
    assert.match(depsFailure.result.stderr, /failed lane\(s\): rust$/m);
    rmSync(log);
    // A lane that fails before the browser suite must not leave the install
    // running: verify stops and reaps it on exit (#244).
    const hangPid = join(fakeDir, "deps.pid");
    // A leaked install would hold verify's output open for its full 600 s.
    const earlyFailure = runVerify(
      { TRELLIS_BROWSER_SYSTEM_DEPS: "1", FAKE_VERIFY_MISSING_LIBS: "1", FAKE_VERIFY_HANG_DEPS: hangPid, FAKE_VERIFY_FAIL_CLIPPY: "1" },
      120_000,
    );
    assert.equal(earlyFailure.result.error, undefined, "verify did not exit after its lane failed");
    assert.equal(earlyFailure.result.status, 23);
    const installer = Number(readFileSync(hangPid, "utf8"));
    // The stopped install may still be a zombie that init has yet to reap, and
    // kill(pid, 0) succeeds on a zombie; it holds nothing open, so it counts as stopped.
    const state = (spawnSync("ps", ["-o", "stat=", "-p", String(installer)], { encoding: "utf8" }).stdout ?? "").trim();
    if (state === "") {
      // No ps row (or ps failed): the process itself must be gone.
      assert.throws(() => process.kill(installer, 0), { code: "ESRCH" }, "the background install outlived verify");
    } else {
      assert.ok(state.startsWith("Z"), `the background install outlived verify (state ${state})`);
    }
    rmSync(log);
    const failure = runVerify({ FAKE_VERIFY_FAIL_CLIPPY: "1" });
    assert.equal(failure.result.status, 23);
    assertRan(failure.commands, { ...lanes, rust: lanes.rust.slice(0, 2) });
    assert.match(failure.result.stderr, /failed lane\(s\): rust$/m);
    assert.match(failure.result.stdout, /^==> verify lane rust \(exit 23\)$/m);
    rmSync(log);
    const semanticFailure = runVerify({ FAKE_VERIFY_FAIL_SEMANTIC: "1" });
    assert.equal(semanticFailure.result.status, 29);
    assertRan(semanticFailure.commands, lanes);
    assert.match(semanticFailure.result.stderr, /failed lane\(s\): semantic$/m);
    rmSync(log);
    const bothFailure = runVerify({ FAKE_VERIFY_FAIL_CLIPPY: "1", FAKE_VERIFY_FAIL_SEMANTIC: "1" });
    assert.equal(bothFailure.result.status, 23);
    assertRan(bothFailure.commands, { ...lanes, rust: lanes.rust.slice(0, 2) });
    assert.match(bothFailure.result.stderr, /failed lane\(s\): rust semantic$/m);
    rmSync(log);
    const noNextest = runVerify({ FAKE_VERIFY_NO_NEXTEST: "1" });
    assert.equal(noNextest.result.status, 127);
    assertRan(noNextest.commands, { ...lanes, tests: lanes.tests.slice(0, 1) });
    assert.match(noNextest.result.stderr, /failed lane\(s\): tests$/m);
    assert.match(noNextest.result.stdout, /cargo-nextest is required/);
    rmSync(log);
    // GHSA-vc25-qg58-w57c: an affected nextest is refused before any test runs.
    const oldNextest = runVerify({ FAKE_VERIFY_NEXTEST_VERSION: "0.9.148" });
    assert.equal(oldNextest.result.status, 127);
    assertRan(oldNextest.commands, { ...lanes, tests: lanes.tests.slice(0, 1) });
    assert.match(oldNextest.result.stdout, /cargo-nextest 0\.9\.148 is affected by GHSA-vc25-qg58-w57c/);
    rmSync(log);
    // CI runs lanes as separate jobs: --lane selects lanes, and the rust lane's
    // halves (lint, then browser) can each run alone.
    const [lint, browser] = [lanes.rust.slice(0, 3), lanes.rust.slice(3)];
    const testsOnly = runVerify({ TRELLIS_BROWSER_SYSTEM_DEPS: "1" }, undefined, ["--lane", "tests"]);
    assert.equal(testsOnly.result.status, 0, testsOnly.output);
    // No browser suite, so no system package install.
    assertRan(testsOnly.commands, { setup: lanes.setup, tests: lanes.tests });
    assert.doesNotMatch(testsOnly.output, /==> verify lane (rust|semantic|cohorts|checks)/);
    rmSync(log);
    const split = runVerify({}, undefined, ["--lane", "lint", "--lane=semantic", "--lane", "cohorts", "--lane", "checks"]);
    assert.equal(split.result.status, 0, split.output);
    assertRan(split.commands, { setup: lanes.setup, lint, semantic: lanes.semantic, cohorts: lanes.cohorts, checks: lanes.checks });
    rmSync(log);
    const browserOnly = runVerify({ TRELLIS_BROWSER_SYSTEM_DEPS: "1" }, undefined, ["--lane", "browser"]);
    assert.equal(browserOnly.result.status, 0, browserOnly.output);
    assertRan(browserOnly.commands, { setup: lanes.setup, browser, deps: [libs] });
    rmSync(log);
    const failedHalf = runVerify({ FAKE_VERIFY_FAIL_CLIPPY: "1" }, undefined, ["--lane", "lint", "--lane", "tests"]);
    assert.equal(failedHalf.result.status, 23);
    assertRan(failedHalf.commands, { setup: lanes.setup, lint: lint.slice(0, 2), tests: lanes.tests });
    assert.match(failedHalf.result.stderr, /failed lane\(s\): lint$/m);
    rmSync(log);
    // Bad selections fail before setup runs anything.
    for (const args of [["--lane", "nope"], ["--lane"], ["--bogus"], ["--lane", "rust", "--lane", "lint"]]) {
      const refused = runVerify({}, undefined, args);
      assert.equal(refused.result.status, 2, `${args.join(" ")}: ${refused.output}`);
      assert.deepEqual(refused.commands, [], args.join(" "));
    }
  } finally {
    rmSync(fakeDir, { recursive: true, force: true });
  }
});

test("verify cancellation rechecks recorded identities after descending, without PID fallback", () => {
  const source = readFileSync(join(ROOT, "tools/verify"), "utf8");
  const functions = source.slice(source.indexOf("process_identity()"), source.indexOf("# An early lane failure"));
  const temp = mkdtempSync(join(tmpdir(), "trellis-verify-identity-"));
  try {
    const rootIdentity = "99 Mon Nov 2 10:00:00 2026 installer";
    const childIdentity = "123 Mon Nov 2 10:00:00 2026 child";
    writeFileSync(join(temp,"123"),rootIdentity+"\n");
    writeFileSync(join(temp,"124"),childIdentity+"\n");
    const script = `${functions}
process_identity() { cat "$FIXTURE/$1" 2>/dev/null || true; }
pgrep() {
  if [ "$2" = 123 ]; then echo 124; fi
  if [ "$2" = 124 ]; then
    printf '%s\n' '99 Mon Nov 2 10:00:01 2026 replacement' > "$FIXTURE/123"
  fi
}
kill() { printf '%s\n' "$*" >> "$FIXTURE/signals"; }
stop_tree 123 '${rootIdentity}'
stop_tree 125 ''
stop_tree 124 'wrong identity'
`;
    const result = spawnSync("/bin/sh", ["-c",script], {encoding:"utf8", env:{...process.env,FIXTURE:temp}});
    assert.equal(result.status,0,result.stderr);
    assert.equal(readFileSync(join(temp,"signals"),"utf8"),"-TERM 124\n");
  } finally { rmSync(temp,{recursive:true,force:true}); }
});
