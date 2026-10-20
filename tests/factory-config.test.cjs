const assert = require("node:assert/strict");
const { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { delimiter, join } = require("node:path");
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
  if [ "$arg" = tests/browser/saved-items.test.cjs ] && [ -z "\${TRELLIS_SUITE_BINARY:-}" ]; then exit 37; fi
done
if [ "\${FAKE_VERIFY_NO_NEXTEST:-0}" = 1 ] && [ "$1" = nextest ]; then exit 101; fi
if [ "\${FAKE_VERIFY_FAIL_CLIPPY:-0}" = 1 ] && [ "$1" = clippy ]; then exit 23; fi
if [ "\${FAKE_VERIFY_FAIL_SEMANTIC:-0}" = 1 ] && [ "$1" = tools/semantic-contract/test/run.mjs ]; then exit 29; fi
`;
  try {
    for (const name of ["cargo", "node"]) {
      const command = join(fakeDir, name);
      writeFileSync(command, fakeTool);
      chmodSync(command, 0o755);
    }
    function runVerify(extraEnv = {}) {
      const result = spawnSync(config.verify, [], {
        cwd: ROOT,
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${fakeDir}${delimiter}${process.env.PATH ?? ""}`,
          FAKE_VERIFY_LOG: log,
          FAKE_BROWSER_ROOT: join(fakeDir, "browser-cache"),
          ...extraEnv,
        },
      });
      const commands = readFileSync(log, "utf8").trimEnd().split("\n").map((line) => line.split("\t"));
      return { result, commands };
    }
    // Lanes run concurrently; commands within a lane keep this order.
    const lanes = {
      setup: [["node", "tools/verify-env.mjs"]],
      rust: [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
        ["cargo", "clippy", "--locked", "--all-targets", "--features", "test-support", "--", "-D", "warnings"],
        ["cargo", "build", "--locked", "--bin", "trellis"],
        ["node", "--test", "--test-concurrency=1", "tests/browser/saved-items.test.cjs", "tests/browser/sequence-pele.test.cjs", "tests/browser/sequence.test.cjs", "tests/browser/sequence-cost.test.cjs"],
      ],
      tests: [
        ["cargo", "nextest", "--version"],
        ["cargo", "nextest", "run", "--locked", "--all-targets", "--no-fail-fast", "--features", "test-support"],
      ],
      semantic: [["node", "tools/semantic-contract/test/run.mjs"]],
      cohorts: [["node", "--test", "tools/synthetic-cohorts/test/cohorts.test.mjs"]],
      checks: [
        ["node", "--test", "runtime/acp/runner.test.mjs"],
        ["node", "--test", "tests/mcp-catalog-sdk.test.mjs"],
        ["node", "--test", "tests/factory-config.test.cjs"],
        ["node", "docs/semantic-evidence/check-policy.mjs"],
        ["node", "--check", "web/app.js"],
        ["node", "--test", "tests/question-ui.test.cjs", "tests/browse-ui.test.cjs", "tests/sequence-ui.test.cjs", "tests/token-ui.test.cjs", "tests/external-source-ui.test.cjs", "tests/dependency-ui.test.cjs", "tests/shell-ui.test.cjs", "tests/classes-ui.test.cjs", "tests/navigation-ui.test.cjs"],
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
    assert.equal(success.result.status, 0, success.result.stderr);
    assertRan(success.commands, lanes);
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
  } finally {
    rmSync(fakeDir, { recursive: true, force: true });
  }
});
