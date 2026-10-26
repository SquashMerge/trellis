import test from "node:test";
import assert from "node:assert/strict";
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { loadFixture } from "../load.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";

// Decision 0017: executable and toolchain captures may be attested by the
// reference hardware profile instead of stored bytes.
const example = fileURLToPath(
  new URL(
    "../../../tests/fixtures/semantic-evidence/v1/example/",
    import.meta.url,
  ),
);
const profilePath = "docs/semantic-evidence/hardware/test-v1.json";
const baseline = await loadFixture(example);
const hashOf = (ref) =>
  baseline.fixture.captures.find((x) => x.ref === ref).hash;

// Copy the example, replace the P executable and toolchain captures with pins,
// and write a test profile into a temporary repository root.
async function pinnedExample(t, edit = () => {}) {
  const scratch = await mkdtemp(join(tmpdir(), "pinned-"));
  t.after(() => rm(scratch, { recursive: true, force: true }));
  const root = join(scratch, "fixtures", "example");
  const repository = join(scratch, "repo");
  await cp(example, root, { recursive: true });
  const fixture = JSON.parse(await readFile(join(root, "fixture.json")));
  const profile = {
    semanticProducers: [{ name: "P", executableHash: hashOf("P"), positionEncoding: "utf8" }],
    toolchains: [{ name: "tool", toolchainHash: hashOf("tool") }],
  };
  for (const capture of fixture.captures)
    if (capture.ref === "P" || capture.ref === "tool") {
      await rm(join(root, capture.file));
      delete capture.file;
      capture.pinned = `${profilePath}#${capture.ref}`;
    }
  edit(fixture, profile);
  await writeFile(join(root, "fixture.json"), JSON.stringify(fixture));
  await mkdir(join(repository, "docs/semantic-evidence/hardware"), {
    recursive: true,
  });
  await writeFile(join(repository, profilePath), JSON.stringify(profile));
  return () => loadFixture(root, { repository });
}
const capture = (fixture, ref) => fixture.captures.find((x) => x.ref === ref);

test("PIN.ADMIT a pinned executable and toolchain capture pass like stored bytes", async (t) => {
  const loaded = await (await pinnedExample(t))();
  assert.equal(loaded.captureBytes.has("P"), false);
  assert.equal(loaded.captureBytes.has("tool"), false);
  const normalized = normalizeFixture(loaded);
  const expected = normalizeFixture(baseline);
  assert.deepEqual(normalized.records, expected.records);
  // Coverage re-checks every captured producer executable (Decision 0017).
  assert.doesNotThrow(() => checkCoverage(loaded, normalized.records));
});
test("PIN.DIGEST rejects a pinned hash that differs from the profile", async (t) => {
  const load = await pinnedExample(t, (_, profile) => {
    profile.semanticProducers[0].executableHash = "0".repeat(64);
  });
  await assert.rejects(
    load(),
    (error) =>
      error.assertion === "IDENTITY.DIGEST" &&
      error.message.includes("pinned digest differs"),
  );
});
test("PIN.ENCODING rejects a pinned producer convention mismatch", async (t) => {
  const load = await pinnedExample(t, (_, profile) => {
    profile.semanticProducers[0].positionEncoding = "utf16";
  });
  await assert.rejects(load(), {
    assertion: "IDENTITY.PRODUCER",
    message: /pinned position encoding differs/,
  });
});
test("PIN.ENCODING checks comparison-only producers using pinned executable bytes", async (t) => {
  const load = await pinnedExample(t, (fixture) => {
    fixture.comparison.producers.push({
      ...fixture.comparison.producers.find((producer) => producer.id === "P"),
      id: "comparison-only",
      positionEncoding: "utf16",
    });
  });
  await assert.rejects(load(), {
    assertion: "IDENTITY.PRODUCER",
    message: /pinned position encoding differs/,
  });
});
test("PIN.ENCODING checks every producer sharing the pinned executable hash", async (t) => {
  const load = await pinnedExample(t, (fixture) => {
    fixture.producers.find((producer) => producer.id === "Q").executableHash = hashOf("P");
    fixture.producers.find((producer) => producer.id === "Q").positionEncoding = "utf16";
  });
  await assert.rejects(load(), {
    assertion: "IDENTITY.PRODUCER",
    message: /pinned position encoding differs/,
  });
});
test("PIN.ENTRY rejects a pin naming a missing profile entry or profile", async (t) => {
  const missingEntry = await pinnedExample(t, (fixture) => {
    capture(fixture, "P").pinned = `${profilePath}#scip-nothing`;
  });
  await assert.rejects(missingEntry(), {
    assertion: "IDENTITY.DIGEST",
    message: /no semanticProducers entry/,
  });
  const missingProfile = await pinnedExample(t, (fixture) => {
    capture(fixture, "P").pinned =
      "docs/semantic-evidence/hardware/absent.json#P";
  });
  await assert.rejects(missingProfile(), { assertion: "IDENTITY.INVENTORY" });
  const outside = await pinnedExample(t, (fixture) => {
    capture(fixture, "P").pinned = "docs/other.json#P";
  });
  await assert.rejects(outside(), { assertion: "IDENTITY.PATH" });
});
test("PIN.SHAPE requires exactly one of file or pinned", async (t) => {
  const both = await pinnedExample(t, (fixture) => {
    capture(fixture, "P").file = "captures/P.bin";
  });
  await assert.rejects(both(), { assertion: "FORMAT.SHAPE" });
  const neither = await pinnedExample(t, (fixture) => {
    delete capture(fixture, "P").pinned;
  });
  await assert.rejects(neither(), { assertion: "FORMAT.SHAPE" });
});
test("PIN.KIND rejects pinning a config, dependency or semantic artifact", async (t) => {
  for (const ref of ["config", "deps", "semantic-r0"]) {
    const load = await pinnedExample(t, (fixture) => {
      const row = capture(fixture, ref);
      delete row.file;
      row.pinned = `${profilePath}#P`;
    });
    await assert.rejects(load(), { assertion: "FORMAT.SHAPE" }, ref);
  }
});
