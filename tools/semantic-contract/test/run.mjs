#!/usr/bin/env node
import { cp, mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { discoverFixtures } from "../load.mjs";
import { generateFixture, checkPublication } from "../publish.mjs";

const here = fileURLToPath(new URL(".", import.meta.url));
const project = resolve(here, "../../..");
const defaultRoot = join(project, "tests/fixtures/semantic-evidence/v1");
const shared = [
  "author-helper",
  "anchors",
  "answers",
  "coordinates",
  "counts",
  "example-fixture",
  "formats",
  "freshness",
  "graph-evidence",
  "graph",
  "identity",
  "load",
  "lookup",
  "normalization",
  "pinned-captures",
  "publication",
  "record-bindings",
  "record-coverage",
  "record-joins",
  "record-measurement",
  "record-relationships",
  "warnings",
  "fixture",
].map((name) => join(here, `${name}.test.mjs`));

export function parseRunnerArgs(args) {
  if (args.length === 0) return defaultRoot;
  if (
    args.length !== 2 ||
    args[0] !== "--fixtures-root" ||
    !args[1] ||
    args[1].startsWith("--")
  )
    throw new Error(
      "RUNNER.ARGS fixtures-root: expected --fixtures-root <path>",
    );
  return resolve(args[1]);
}
function fail(assertion, field, message) {
  const error = new Error(`${assertion} ${field}: ${message}`);
  Object.assign(error, { assertion, code: "invalidRecord", field });
  throw error;
}
async function publicationBytes(root, manifest) {
  return Promise.all(
    [
      "manifest.json",
      ...["records", "answers", "counts"].map((name) => manifest[name].path),
    ].map((path) => readFile(join(root, "generated", path))),
  );
}
async function tree(root) {
  const entries = [];
  async function visit(directory, prefix = "") {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const name = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) await visit(join(directory, entry.name), name);
      else entries.push([name, await readFile(join(directory, entry.name))]);
    }
  }
  await visit(root);
  return entries.sort((a, b) =>
    Buffer.compare(Buffer.from(a[0]), Buffer.from(b[0])),
  );
}
function sameTree(a, b) {
  return (
    a.length === b.length &&
    a.every(
      ([name, bytes], index) =>
        name === b[index][0] && bytes.equals(b[index][1]),
    )
  );
}
export async function runFixtures(root) {
  const fixtures = await discoverFixtures(root);
  if (fixtures.length === 0)
    fail("RUNNER.DISCOVERY", "fixtures-root", `no fixtures found in ${root}`);
  for (const fixture of fixtures) {
    const temporary = await mkdtemp(join(tmpdir(), "trellis-semantic-runner-"));
    const copy = join(temporary, basename(fixture));
    try {
      const { profile } = JSON.parse(
        await readFile(join(fixture, "fixture.json"), "utf8"),
      );
      console.log(
        profile === "draft"
          ? `Checking fixture ${fixture} (DRAFT: count floors not enforced)`
          : `Checking fixture ${fixture}`,
      );
      await cp(fixture, copy, { recursive: true });
      const first = await generateFixture(copy);
      const original = await publicationBytes(copy, first);
      const before = await tree(copy);
      await generateFixture(copy, { check: true });
      await checkPublication(copy);
      const after = await tree(copy);
      if (!sameTree(before, after))
        fail(
          "RUNNER.CHECK",
          "generated",
          `check commands wrote fixture ${fixture}`,
        );
      const second = await generateFixture(copy);
      const regenerated = await publicationBytes(copy, second);
      if (
        JSON.stringify(first) !== JSON.stringify(second) ||
        original.some((bytes, index) => !bytes.equals(regenerated[index]))
      )
        fail(
          "RUNNER.BYTES",
          "generated",
          `non-deterministic generation for ${fixture}`,
        );
      // A committed publication, when present, must match the independently checked copy.
      if ((await readdir(fixture)).includes("generated"))
        await checkPublication(fixture);
    } catch (error) {
      error.message = `Fixture ${fixture}: ${error.message}`;
      throw error;
    } finally {
      await rm(temporary, { recursive: true, force: true });
    }
  }
  return fixtures.length;
}
export async function main(args = process.argv.slice(2)) {
  const root = parseRunnerArgs(args);
  const tested = spawnSync(process.execPath, ["--test", ...shared], {
    cwd: project,
    env: { ...process.env, SEMANTIC_FIXTURES_ROOT: root },
    stdio: "inherit",
  });
  if (tested.error) throw tested.error;
  if (tested.status !== 0)
    fail(
      "RUNNER.TESTS",
      "shared",
      `node --test failed with exit ${tested.status ?? tested.signal}`,
    );
  console.log(`Checked ${await runFixtures(root)} discovered fixture(s)`);
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  main().catch((error) => {
    console.error(
      `${error.assertion ?? "RUNNER.ERROR"} ${error.code ?? "invalidRecord"} ${error.field ?? "runner"}: ${error.message}`,
    );
    process.exitCode = 1;
  });
