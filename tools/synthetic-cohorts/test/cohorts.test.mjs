import "./composition.test.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync, readdirSync, rmSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const generator = join(here, "..", "generate.mjs");
const pin = join(here, "..", "manifest-v2.json");
const expected = {
  small: { files: 100, sourceBytes: 1677722, each: [419431, 419431, 419430, 419430], count: 25 },
  medium: { files: 1000, sourceBytes: 16777216, each: [4194304, 4194304, 4194304, 4194304], count: 250 },
  large: { files: 10000, sourceBytes: 134217728, each: [33554432, 33554432, 33554432, 33554432], count: 2500 },
};
const languages = [["java", "java"], ["rust", "rs"], ["python", "py"], ["javascript", "js"]];

function run(args) {
  return spawnSync(process.execPath, [generator, ...args], { encoding: "utf8", timeout: 180000 });
}

function inventory(root) {
  const found = [];
  function walk(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else {
        assert.ok(entry.isFile(), `unexpected non-file ${path}`);
        found.push(relative(root, path).split(/[/\\]/).join("/"));
      }
    }
  }
  walk(root);
  return found.sort();
}

// Inspect the disk without importing generator code or trusting its manifest hashes.
function verify(root, manifestBytes) {
  const manifest = JSON.parse(manifestBytes);
  assert.equal(manifest.version, 2);
  assert.equal(manifest.seed, "trellis-synthetic-cohorts-v1");
  assert.equal(manifest.hashAlgorithm, "sha256");
  assert.deepEqual(Object.keys(manifest.totals), Object.keys(expected));
  for (const [size, values] of Object.entries(expected))
    assert.deepEqual(manifest.totals[size], { files: values.files, sourceBytes: values.sourceBytes });
  assert.equal(manifest.files.length, 11100);
  const paths = manifest.files.map((entry) => entry.path);
  assert.deepEqual(paths, [...new Set(paths)].sort(), "manifest paths must be sorted and unique");
  const wanted = [];
  for (const [size, values] of Object.entries(expected)) {
    for (const [language, ext] of languages) {
      const rootCount = language !== "rust" ? 0 : size === "small" ? 1 : size === "medium" ? 5 : 40;
      const ordinary = values.count - rootCount;
      for (let i = 0; i < ordinary; i++) {
        const group = language === "rust" && size !== "small" ?
          `g${String(Math.floor(((i + 1) * (rootCount - 1) - 1) / ordinary)).padStart(2, "0")}/` : "";
        wanted.push(`${size}/${language}/${group}C${size}${String(i).padStart(4, "0")}.${ext}`);
      }
      if (rootCount) wanted.push(`${size}/rust/lib.rs`);
      for (let g = 0; g < rootCount - 1; g++)
        wanted.push(`${size}/rust/g${String(g).padStart(2, "0")}/mod.rs`);
    }
  }
  wanted.sort();
  assert.deepEqual(paths, wanted, "exact source-file inventory");
  assert.deepEqual(inventory(root), [...wanted, "manifest.json"].sort(), "actual disk inventory");
  assert.ok(readFileSync(join(root, "manifest.json")).equals(manifestBytes), "manifest bytes");
  const counted = Object.fromEntries(Object.keys(expected).map((size) => [size, { files: 0, sourceBytes: 0, languages: Object.fromEntries(languages.map(([name]) => [name, { files: 0, sourceBytes: 0 }])) }]));
  for (const entry of manifest.files) {
    const [size, language] = entry.path.split("/");
    const bytes = readFileSync(join(root, entry.path));
    assert.equal(entry.sourceBytes, bytes.length, `length ${entry.path}`);
    assert.match(entry.sha256, /^[0-9a-f]{64}$/);
    assert.equal(entry.sha256, createHash("sha256").update(bytes).digest("hex"), `hash ${entry.path}`);
    counted[size].files++;
    counted[size].sourceBytes += bytes.length;
    counted[size].languages[language].files++;
    counted[size].languages[language].sourceBytes += bytes.length;
  }
  for (const [size, values] of Object.entries(expected)) {
    assert.equal(counted[size].files, values.files);
    assert.equal(counted[size].sourceBytes, values.sourceBytes);
    languages.forEach(([language], index) => {
      assert.deepEqual(counted[size].languages[language], { files: values.count, sourceBytes: values.each[index] });
    });
  }
  return manifest;
}

test("CLI demands a fresh explicit destination", () => {
  const temp = mkdtempSync(join(tmpdir(), "trellis-cohorts-args-"));
  try {
    assert.notEqual(run([]).status, 0);
    assert.notEqual(run(["--unknown", join(temp, "wrong")]).status, 0);
    assert.notEqual(run(["--out", temp]).status, 0);
    assert.deepEqual(readdirSync(temp), []);
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
});

test("two cohorts match pinned bytes, hashes, inventory and exact per-language totals", { timeout: 300000 }, () => {
  const a = mkdtempSync(join(tmpdir(), "trellis-cohort-a-"));
  const b = mkdtempSync(join(tmpdir(), "trellis-cohort-b-"));
  try {
    const first = join(a, "corpus");
    const second = join(b, "corpus");
    for (const root of [first, second]) {
      const result = run(["--out", root]);
      assert.equal(result.status, 0, result.stderr || result.error?.message);
    }
    const pinned = readFileSync(pin);
    const one = readFileSync(join(first, "manifest.json"));
    const two = readFileSync(join(second, "manifest.json"));
    assert.ok(one.equals(pinned), "pinned manifest bytes");
    assert.ok(two.equals(one), "two-run manifest bytes");
    const manifest = verify(first, pinned);
    verify(second, pinned);
    for (const { path } of manifest.files)
      assert.deepEqual(readFileSync(join(first, path)), readFileSync(join(second, path)), `repeatability ${path}`);

    const sample = join(first, manifest.files[0].path);
    const original = readFileSync(sample);
    function expectRejected(change, restore) {
      try {
        change();
        assert.throws(() => verify(first, pinned), /inventory|length|hash/);
      } finally {
        restore();
      }
      verify(first, pinned);
    }
    expectRejected(() => unlinkSync(sample), () => writeFileSync(sample, original));
    const unexpected = join(first, "small", "java", "unexpected.java");
    expectRejected(() => writeFileSync(unexpected, "// extra\n"), () => rmSync(unexpected, { force: true }));
    expectRejected(() => { const changed = Buffer.from(original); changed[0] ^= 1; writeFileSync(sample, changed); }, () => writeFileSync(sample, original));
    expectRejected(() => writeFileSync(sample, Buffer.concat([original, Buffer.from("x")])), () => writeFileSync(sample, original));
  } finally {
    rmSync(a, { recursive: true, force: true });
    rmSync(b, { recursive: true, force: true });
  }
});
