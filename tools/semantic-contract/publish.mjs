import {
  readFile,
  writeFile,
  mkdir,
  mkdtemp,
  rename,
  rm,
  lstat,
} from "node:fs/promises";
import { join } from "node:path";
import { loadFixture, confinedFile } from "./load.mjs";
import { normalizeFixture } from "./normalize.mjs";
import { materializeAnswers } from "./answers.mjs";
import { checkAnswers } from "./graph-check.mjs";
import { checkCounts } from "./counts.mjs";
import { checkCoverage } from "./record-check/coverage.mjs";
import { validate } from "./formats.mjs";
import { parseJson, canonicalBytes } from "./json.mjs";
import { contentHash } from "./identity.mjs";

const names = ["records", "answers", "counts"];
const order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
function fail(assertion, field, message) {
  const error = new Error(`${assertion} ${field}: ${message}`);
  Object.assign(error, { assertion, field, code: "invalidRecord" });
  throw error;
}
async function present(path) {
  try {
    return await lstat(path);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}
function inputPaths(fixture) {
  const paths = [
    "fixture.json",
    ...fixture.revisions.flatMap((revision) =>
      revision.documents.map((document) => document.sourceFile),
    ),
    ...fixture.captures.flatMap((capture) => capture.file ?? []),
    fixture.nativeArtifact,
    ...fixture.semanticArtifacts,
    ...fixture.annotationFiles,
    fixture.answersFile,
    fixture.dispositionsFile,
    fixture.anchorCasesFile,
  ];
  return [...new Set(paths)].sort(order);
}
export async function inputDigest(root, fixture) {
  const rows = [];
  for (const path of inputPaths(fixture))
    rows.push({ path, hash: contentHash(await confinedFile(root, path)) });
  validate("InputDigestInput", rows);
  return contentHash(canonicalBytes(rows));
}

// Compilation deliberately checks authored answers before any output is staged.
export async function compileFixture(root) {
  const loaded = await loadFixture(root);
  const normalized = normalizeFixture(loaded);
  const records = normalized.records;
  const answers = materializeAnswers(loaded.answers, normalized);
  checkAnswers(loaded, records, checkCoverage(loaded, records), answers);
  const counts = checkCounts(loaded, records, loaded.dispositions);
  const values = { records, answers, counts };
  const bytes = Object.fromEntries(
    names.map((name) => [name, canonicalBytes(values[name])]),
  );
  for (const [name, type] of [
    ["records", "NormalizedRecordsV1"],
    ["answers", "AnswersV1"],
    ["counts", "CountsV1"],
  ])
    validate(type, parseJson(bytes[name]));
  const hashes = Object.fromEntries(
    names.map((name) => [name, contentHash(bytes[name])]),
  );
  const inputHash = await inputDigest(root, loaded.fixture);
  const digest = {
    recordsHash: hashes.records,
    answersHash: hashes.answers,
    countsHash: hashes.counts,
    inputHash,
  };
  validate("BundleDigestInput", digest);
  const bundleHash = contentHash(canonicalBytes(digest));
  const manifest = {
    formatVersion: 1,
    contractVersion: 1,
    warningsVersion: "warnings-v1",
    language: loaded.fixture.language,
    profile: loaded.fixture.profile,
    inputHash,
    bundleHash,
    ...Object.fromEntries(
      names.map((name) => [
        name,
        { path: `bundles/${bundleHash}/${name}.json`, hash: hashes[name] },
      ]),
    ),
  };
  validate("ManifestV1", manifest);
  return { loaded, values, bytes, manifest };
}

async function readPublished(root, expected) {
  const target = join(root, "generated", "manifest.json");
  if (!(await present(target)))
    fail("PUBLICATION.MANIFEST", "manifest", "published manifest missing");
  let manifest, manifestBytes;
  try {
    manifestBytes = await readFile(target);
    manifest = parseJson(manifestBytes);
    validate("ManifestV1", manifest);
  } catch (error) {
    fail("PUBLICATION.MANIFEST", "manifest", error.message);
  }
  const digest = {
    recordsHash: manifest.records.hash,
    answersHash: manifest.answers.hash,
    countsHash: manifest.counts.hash,
    inputHash: manifest.inputHash,
  };
  const bundleHash = contentHash(canonicalBytes(digest));
  if (bundleHash !== manifest.bundleHash)
    fail(
      "PUBLICATION.BUNDLE",
      "bundleHash",
      `manifest bundle digest differs from its linked hashes: expected ${bundleHash}, actual ${manifest.bundleHash}`,
    );
  for (const name of names) {
    const descriptor = manifest[name];
    if (descriptor.path !== `bundles/${manifest.bundleHash}/${name}.json`)
      fail("PUBLICATION.PATH", name, "published bundle path differs");
    const file = join(root, "generated", descriptor.path);
    let bytes;
    try {
      bytes = await readFile(file);
    } catch (error) {
      fail(
        "PUBLICATION.FILE",
        name,
        `published file missing: ${error.message}`,
      );
    }
    const actualHash = contentHash(bytes);
    if (actualHash !== descriptor.hash)
      fail(
        "PUBLICATION.HASH",
        name,
        `published byte hash differs: expected ${descriptor.hash}, actual ${actualHash}`,
      );
  }
  if (!manifestBytes.equals(canonicalBytes(expected)))
    fail(
      "PUBLICATION.MANIFEST",
      "manifest",
      `published manifest bytes differ from admitted inputs: expected ${contentHash(canonicalBytes(expected))}, actual ${contentHash(manifestBytes)}`,
    );
  return manifest;
}

export async function checkPublication(root) {
  const built = await compileFixture(root);
  await readPublished(root, built.manifest);
  for (const name of names) {
    const actual = await readFile(
      join(root, "generated", built.manifest[name].path),
    );
    if (!actual.equals(built.bytes[name]))
      fail(
        "PUBLICATION.BYTES",
        name,
        `published bytes differ from checked canonical result: expected ${contentHash(built.bytes[name])}, actual ${contentHash(actual)}`,
      );
  }
  return built.manifest;
}

async function verifyBundle(directory, built) {
  for (const name of names) {
    let actual;
    try {
      actual = await readFile(join(directory, `${name}.json`));
    } catch (error) {
      fail(
        "PUBLICATION.IMMUTABLE",
        name,
        `bundle file missing: ${error.message}`,
      );
    }
    if (!actual.equals(built.bytes[name]))
      fail(
        "PUBLICATION.IMMUTABLE",
        name,
        `bundle bytes conflict with checked canonical output: expected ${built.manifest[name].hash}, actual ${contentHash(actual)}`,
      );
  }
}

// Content-addressed bundles are immutable. The manifest is renamed into place
// last, so a failure before that leaves the previous publication intact.
export async function generateFixture(root, { check = false } = {}) {
  if (check) return checkPublication(root);
  const built = await compileFixture(root);
  const generated = join(root, "generated"),
    final = join(generated, "bundles", built.manifest.bundleHash);
  await mkdir(join(generated, "bundles"), { recursive: true });
  const temporary = await mkdtemp(join(generated, ".publish-"));
  try {
    if (await present(final)) await verifyBundle(final, built);
    else {
      const staged = join(temporary, "bundle");
      await mkdir(staged);
      for (const name of names)
        await writeFile(join(staged, `${name}.json`), built.bytes[name], {
          flag: "wx",
        });
      try {
        await rename(staged, final);
      } catch (error) {
        // A concurrent generation installed this content-addressed bundle first.
        if (error.code !== "ENOTEMPTY" && error.code !== "EEXIST") throw error;
        await verifyBundle(final, built);
      }
    }
    const manifestFile = join(temporary, "manifest.json");
    await writeFile(manifestFile, canonicalBytes(built.manifest), {
      flag: "wx",
    });
    await rename(manifestFile, join(generated, "manifest.json"));
    return built.manifest;
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
