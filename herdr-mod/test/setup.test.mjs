// setup-viewer.sh against a stand-in mini: `ssh` is a script that runs the remote command in a
// temporary "remote" home, and herdr-app is not on PATH. No network, no Herdr app.
//   node --test herdr-mod/test/setup.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const script = join(dirname(fileURLToPath(import.meta.url)), "..", "setup-viewer.sh");
const put = (path, text) => { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, text); };

function world() {
  const dir = mkdtempSync(join(tmpdir(), "trellis-setup-"));
  const remote = join(dir, "remote"), app = join(dir, "viewer", "app"), bin = join(dir, "bin");
  // What the mini has after `node build.mjs --install` and the trellis-files clone, plus the
  // app's own per-machine files, which must stay behind.
  for (const [path, text] of Object.entries({
    "mods/trellis/main.js": "new trellis", "mods/trellis/mod.json": "{}", "mods/trellis/setup-viewer.sh": "#",
    "mods/trellis/.render/architecture-x.svg": "<svg/>", "mods/trellis/gpui-kit.d.ts": "d", "mods/trellis/jsconfig.json": "{}",
    "mods/trellis-files/main.js": "new files", "mods/trellis-files/mod.json": "{}", "mods/trellis-files/herdr.d.ts": "d",
    "layouts/trellis.json": '{"id":"root","new":true}', "layouts/main.json": "remote main",
  })) put(join(remote, ".config/herdr-app", path), text);
  put(join(bin, "ssh"), `#!/bin/sh\nshift\ncd "${remote}" && exec sh -c "$*"\n`);
  chmodSync(join(bin, "ssh"), 0o755);
  const run = (...args) => spawnSync("sh", [script, ...args], {
    encoding: "utf8", stdin: "ignore",
    env: { PATH: `${bin}:/usr/bin:/bin`, HOME: join(dir, "viewer"), HERDR_APP_HOME: app, TMPDIR: dir },
  });
  return { dir, remote, app, run, done: () => rmSync(dir, { recursive: true, force: true }) };
}

test("setup-viewer installs both mods and the layout, keeps the old layout, and leaves the rest alone", () => {
  const w = world();
  try {
    put(join(w.app, "layouts/trellis.json"), "old layout");
    put(join(w.app, "mods/files/main.js"), "shipped files");
    put(join(w.app, "layouts/main.json"), "shipped main");
    const first = w.run("mini");
    assert.equal(first.status, 0, first.stderr);
    const read = (p) => readFileSync(join(w.app, p), "utf8");
    assert.equal(read("mods/trellis/main.js"), "new trellis");
    assert.equal(read("mods/trellis-files/main.js"), "new files");
    assert.equal(read("layouts/trellis.json"), '{"id":"root","new":true}');
    assert.equal(read("layouts/trellis.json.bak"), "old layout");
    assert.equal(read("mods/files/main.js"), "shipped files");
    assert.equal(read("layouts/main.json"), "shipped main");
    for (const p of ["mods/trellis/.render", "mods/trellis/gpui-kit.d.ts", "mods/trellis/jsconfig.json", "mods/trellis-files/herdr.d.ts"])
      assert.ok(!existsSync(join(w.app, p)), `${p} was copied`);
    assert.match(first.stdout, /herdr-app isn't on PATH/);

    // Again: the same result, and the backup still holds the owner's layout.
    const second = w.run("mini");
    assert.equal(second.status, 0, second.stderr);
    assert.equal(read("layouts/trellis.json.bak"), "old layout");
    assert.doesNotMatch(second.stdout, /kept the previous layout/);
  } finally { w.done(); }
});

test("setup-viewer fails clearly, and installs nothing, when the mini lacks a piece or the arguments are wrong", () => {
  const w = world();
  try {
    rmSync(join(w.remote, ".config/herdr-app/mods/trellis-files"), { recursive: true });
    const missing = w.run("mini");
    assert.notEqual(missing.status, 0);
    assert.match(missing.stderr, /missing .*mods\/trellis-files\/main\.js/);
    assert.match(missing.stderr, /couldn't copy from mini/);
    assert.ok(!existsSync(w.app), "nothing is installed");

    const none = w.run();
    assert.notEqual(none.status, 0);
    assert.match(none.stderr, /usage: sh setup-viewer\.sh <ssh host of the mini>/);
    const option = w.run("-oProxyCommand=x");
    assert.notEqual(option.status, 0);
    assert.match(option.stderr, /can't start with '-'/);
  } finally { w.done(); }
});
