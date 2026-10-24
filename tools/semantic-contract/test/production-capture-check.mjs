// Integration companion invoked by tests/native_capture.rs with the pinned Rust binary.
import assert from "node:assert/strict";
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { loadFixture } from "../load.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import { checkCounts } from "../counts.mjs";
import { contentHash } from "../identity.mjs";
import { validate } from "../formats.mjs";

const example = fileURLToPath(new URL("../../../tests/fixtures/semantic-evidence/v1/example/", import.meta.url));
const [mode, root, descriptorPath] = process.argv.slice(2);
const json = async (path) => JSON.parse(await readFile(path, "utf8"));
const save = async (path, value) => {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, JSON.stringify(value, null, 2) + "\n");
};
if (mode === "prepare") {
  const original = await json(join(example, "fixture.json"));
  const revision = original.revisions[0];
  const fixture = {
    ...original, profile: "draft", revisions: [revision],
    producers: [original.producers[0]],
    comparison: { sourceSetId: revision.sourceSetId, revisionId: revision.id, producers: [original.producers[0]] },
    semanticArtifacts: [], annotationFiles: [], coverageIntents: [],
    captures: original.captures.filter((row) => ["toolchain", "config", "dependency"].includes(row.kind)),
  };
  for (const path of [...revision.documents.map((row) => row.sourceFile), ...fixture.captures.map((row) => row.file)]) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await copyFile(join(example, path), join(root, path));
  }
  await mkdir(join(root, "captures"), { recursive: true });
  await save(join(root, fixture.answersFile), { formatVersion: 1, answers: [] });
  await save(join(root, fixture.dispositionsFile), { formatVersion: 1, assertions: [], callableValueNegatives: [] });
  await save(join(root, fixture.anchorCasesFile), { formatVersion: 1, cases: [] });
  await save(join(root, "fixture.json"), fixture);
} else if (mode === "check" || mode === "check-all") {
  const descriptor = await json(descriptorPath);
  const producer = descriptor.producer;
  assert.equal(producer.id, "trellis.native.syntax");
  assert.equal(producer.version, "native-v8");
  assert.equal(producer.positionEncoding, "utf8");
  assert.equal(descriptor.hashKind, "nativeExtractor");
  const fixture = await json(join(root, "fixture.json"));
  if (mode === "check-all") {
    Object.assign(fixture, { formatVersion: 1, profile: "draft", language: fixture.revisions[0].documents[0].key.language,
      captures: [], annotationFiles: [], coverageIntents: [], semanticArtifacts: [],
      nativeArtifact: "native.json", answersFile: "expected/answers.json",
      dispositionsFile: "expected/dispositions.json", anchorCasesFile: "expected/anchors.json",
      comparison: { sourceSetId: "main", revisionId: "r0", producers: [] } });
    for (const [kind, field] of [["toolchain", "toolchainHash"], ["config", "configHash"], ["dependency", "dependencyHash"]]) {
      const file = `captures/${kind}.txt`;
      await mkdir(dirname(join(root, file)), { recursive: true });
      const bytes = Buffer.from(`integration ${kind} capture\n`);
      await writeFile(join(root, file), bytes);
      fixture.captures.push({ ref: kind, kind, file, hash: contentHash(bytes) });
      fixture.revisions[0][field] = contentHash(bytes);
    }
    await save(join(root, fixture.answersFile), { formatVersion: 1, answers: [] });
    await save(join(root, fixture.dispositionsFile), { formatVersion: 1, assertions: [], callableValueNegatives: [] });
    await save(join(root, fixture.anchorCasesFile), { formatVersion: 1, cases: [] });
  }
  fixture.producers = [producer];
  fixture.comparison.producers = [producer];
  const profilePath = "docs/semantic-evidence/hardware/test-native.json";
  const repository = join(dirname(root), "repository");
  await save(join(repository, profilePath), { semanticProducers: [{ name: "production-native", executableHash: producer.executableHash }] });
  fixture.captures.push({ ref: "native", kind: "executable", pinned: `${profilePath}#production-native`, hash: producer.executableHash });
  const native = await json(join(root, fixture.nativeArtifact));
  validate("NativeArtifact", native);
  assert.equal(native.producerId, producer.id);
  assert.deepEqual(native.extractionInputs, []);
  assert.ok(native.declarations.length > 0);
  assert.deepEqual(native.references, []); // Production emits no reference candidates; never synthesize them.
  for (const revision of fixture.revisions) for (const row of revision.documents) {
    const calls = native.calls.filter((call) => call.document.path === row.key.path && call.revisionId === revision.id);
    const requestedRoles = calls.length ? ["call"] : [];
    fixture.coverageIntents.push({ producerId: producer.id, document: row.key, revisionId: revision.id, requestedRoles,
      measurementSupport: ["declarationName", "callee", "invocation", "reference"].map((kind) => ({ kind,
        available: kind !== "reference", diagnostic: kind === "reference" ? "production artifact has no reference candidate family" : null })) });
    const annotationPath = `${row.sourceFile}.annotations.json`;
    fixture.annotationFiles.push(annotationPath);
    await save(join(root, annotationPath), { formatVersion: 1, document: row.key, revisionId: revision.id, scenarios: [], facts: [{
      kind: "coverage", ref: `coverage:${row.key.path}`, record: {
        producerId: producer.id, language: row.key.language, sourceSetId: row.key.sourceSetId, documentPath: row.key.path,
        revisionId: revision.id, requested: calls.length > 0, selected: calls.length > 0,
        state: calls.length ? "complete" : "notRequested", supportedRoles: ["definition", "call"], observedRoles: requestedRoles, diagnostic: null,
      },
    }] });
  }
  await save(join(root, "fixture.json"), fixture);
  const loaded = await loadFixture(root, { repository });
  const normalized = normalizeFixture(loaded);
  checkCoverage(loaded, normalized.records);
  let counts;
  try { counts = checkCounts(loaded, normalized.records); }
  catch (error) { error.message = `${fixture.language}: ${error.message}`; throw error; }
  if (mode === "check-all" && fixture.language === "java") {
    const method = loaded.native.declarations.find((row) => row.kind === "method");
    const module = loaded.native.declarations.find((row) => row.kind === "module");
    assert.ok(method && module);
    assert.notEqual(method.parentRef, module.ref);
    const parent = method.parentRef;
    method.parentRef = module.ref;
    assert.throws(() => checkCounts(loaded, normalized.records), (error) => error.field === "parentRef");
    method.parentRef = parent;
  }
  assert.equal(counts.floorsEnforced, false);
  assert.equal(counts.references, 0);
  assert.equal(counts.scenariosTotal, 0);
  assert.equal(counts.outcomes.find((row) => row.disposition === "unsupported").count, 0);
  console.log(`Production capture normalizes and checks: ${native.declarations.length} declarations, ${native.calls.length} calls`);
} else throw new Error("expected prepare ROOT or check ROOT DESCRIPTOR");
