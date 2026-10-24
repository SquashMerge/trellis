import test from "node:test";
import assert from "node:assert/strict";
import { specimen } from "./load.test.mjs";
import { loadFixture } from "../load.mjs";
import { contentHash, syntaxId } from "../identity.mjs";
import { writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import {
  checkCapturedBasis,
  checkFreshness,
  checkStaleTarget,
  expectedFreshness,
  expectedStaleTarget,
} from "../check-freshness.mjs";
const hash = (x) => contentHash(Buffer.from(x));
function addTargetDeclaration(s, document, revisionId, sourceFile) {
  const artifact = JSON.parse(s.files["captures/native.json"]);
  const bytes = s.files[sourceFile];
  const start = bytes.indexOf("target");
  assert.ok(start >= 0, "target must be present in source bytes");
  artifact.declarations.push({
    ref: `target-${revisionId}`,
    nativeId: null,
    role: "production",
    document,
    revisionId,
    parentRef: null,
    kind: "variable",
    name: "target",
    range: { encoding: "utf8", start: 0, end: Buffer.byteLength(bytes) },
    nameRange: { encoding: "utf8", start, end: start + 6 },
    header: {
      kind: "variable",
      name: "target",
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases: [],
    },
    signature: null,
    witnesses: [
      {
        field: "name",
        witness: {
          range: { encoding: "utf8", start, end: start + 6 },
          text: "target",
        },
      },
    ],
  });
  s.files["captures/native.json"] = JSON.stringify(artifact);
}
function verifiedDeclarations(loaded) {
  const rows = new Map();
  for (const row of loaded.native.declarations) {
    const source = loaded.sources.get(
      JSON.stringify([
        row.document.sourceSetId,
        row.revisionId,
        row.document.path,
      ]),
    );
    assert.ok(source, "declaration source missing");
    assert.equal(
      source.subarray(row.nameRange.start, row.nameRange.end).toString(),
      row.name,
    );
    assert.equal(
      row.witnesses.find((x) => x.field === "name")?.witness.text,
      row.name,
    );
    assert.equal(row.header.kind, row.kind);
    assert.equal(row.header.name, row.name);
    assert.equal(row.parentRef, null, "top-level declaration fixture");
    const siblings = loaded.native.declarations.filter(
      (x) =>
        x.document.path === row.document.path &&
        x.revisionId === row.revisionId &&
        x.document.sourceSetId === row.document.sourceSetId &&
        x.parentRef === null &&
        x.kind === row.kind &&
        x.name === row.name &&
        JSON.stringify(x.signature) === JSON.stringify(row.signature),
    );
    siblings.sort(
      (a, b) => a.range.start - b.range.start || a.range.end - b.range.end,
    );
    const ordinal = siblings.findIndex((x) => x.ref === row.ref);
    assert.ok(ordinal >= 0);
    const id = syntaxId({
      sourceSet: row.document.sourceSetId,
      path: row.document.path,
      language: row.document.language,
      ancestors: [],
      declaration: {
        kind: row.kind,
        name: row.name,
        signature: row.signature,
        ordinal,
      },
    });
    const tuple = JSON.stringify([row.document.sourceSetId, row.revisionId]);
    if (!rows.has(tuple)) rows.set(tuple, new Map());
    rows.get(tuple).set(id, row.document);
  }
  return rows;
}

async function prepared(t) {
  const s = await specimen();
  t.after(s.cleanup);
  const initial = await loadFixture(s.root),
    captured = initial.selected;
  const base = {
    id: "proof1",
    producerId: "semantic",
    document: s.document,
    revisionId: "r1",
    contentHash: captured.documents[0].contentHash,
    evidenceKind: "declarationBinding",
    basis: {
      producerId: "semantic",
      producerVersion: "1",
      producerHash: s.fixture.producers[1].executableHash,
      artifactHash: s.fixture.captures.find((x) => x.ref === "fact").hash,
      language: "javascript",
      sourceSetId: "main",
      revisionId: "r1",
      sourceManifestHash: initial.sourceManifestHash(captured),
      toolchainHash: captured.toolchainHash,
      configHash: captured.configHash,
      dependencyHash: captured.dependencyHash,
      lookupDependencies: [],
    },
    freshness: "fresh",
  };
  const fact = {
    kind: "symbol",
    ref: "symbol1",
    record: {
      key: {
        scheme: "scip",
        symbol: "example/go",
        scope: "global",
        document: null,
      },
      displayName: "go",
      declarations: [],
      provenanceId: "proof1",
    },
  };
  const raw = { formatVersion: 1, producerId: "semantic", facts: [fact] };
  const encoded = JSON.stringify(raw);
  s.files["captures/fact.json"] = encoded;
  const artifactHash = hash(encoded);
  s.fixture.captures.find((x) => x.ref === "fact").hash = artifactHash;
  base.basis.artifactHash = artifactHash;
  s.files["src/go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: s.document,
    revisionId: "r1",
    scenarios: [],
    facts: [{ kind: "provenance", ref: "proof", record: base }, fact],
  });
  await s.flush();
  const r1 = await loadFixture(s.root);
  return { s, r1, base };
}
test("BASIS.* validates each captured descriptor and byte claim before freshness", async (t) => {
  const { r1, base } = await prepared(t);
  assert.ok(checkCapturedBasis(base, r1));
  for (const field of Object.keys(base.basis)) {
    if (field === "lookupDependencies") continue;
    const bad = structuredClone(base);
    bad.basis[field] = field.endsWith("Hash")
      ? "0".repeat(64)
      : field === "language"
        ? "rust"
        : "changed";
    assert.throws(
      () => checkFreshness(bad, r1),
      (error) => error.assertion?.startsWith("BASIS.") === true,
      field,
    );
  }
  const invalidLookup = structuredClone(base);
  invalidLookup.basis.lookupDependencies = ["sid:v1:" + "a".repeat(32)];
  assert.throws(() => checkCapturedBasis(invalidLookup, r1), {
    assertion: "BASIS.LOOKUP_DEPENDENCIES",
  });
  const absent = structuredClone(base);
  absent.basis = null;
  assert.throws(() => checkFreshness(absent, r1), {
    assertion: "BASIS.SEMANTIC_PAIRING",
  });
  const native = {
    ...base,
    producerId: "native",
    evidenceKind: "measuredSyntax",
    basis: null,
  };
  assert.equal(checkFreshness(native, r1), "fresh");
  assert.throws(
    () => checkCapturedBasis({ ...native, basis: base.basis }, r1),
    { assertion: "BASIS.NATIVE_PAIRING" },
  );
});
test("BASIS.RAW_FACT compares captured wrapper fields without depending on JSON key order", async (t) => {
  const { r1, base } = await prepared(t);
  const reordered = {
    ...base,
    basis: Object.fromEntries(Object.entries(base.basis).reverse()),
  };
  assert.equal(checkCapturedBasis(reordered, r1).producer.id, "semantic");
  const wrong = {
    ...reordered,
    basis: { ...reordered.basis, producerVersion: "other" },
  };
  assert.throws(() => checkCapturedBasis(wrong, r1), {
    assertion: "BASIS.PRODUCERVERSION",
    field: "producerVersion",
  });
});
test("FRESHNESS.TWO_REVISION_CONTEXT retains r1 proof but compares pinned r2 snapshot", async (t) => {
  const { s, r1, base } = await prepared(t);
  assert.equal(checkFreshness(base, r1), "fresh");
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    id: "r2",
    documents: [{ key: s.document, revisionId: "r2", sourceFile: "r2/go.js" }],
  });
  s.files["r2/go.js"] = s.source;
  s.fixture.comparison.revisionId = "r2";
  await s.flush();
  const r2 = await loadFixture(s.root);
  assert.equal(expectedFreshness(base, r2), "possiblyStale");
  assert.throws(() => checkFreshness(base, r2), {
    assertion: "FRESHNESS.LABEL",
    field: "freshness",
  });
  const historical = { ...base, freshness: "possiblyStale" };
  assert.equal(checkFreshness(historical, r2), "possiblyStale");
  s.files["r2/go.js"] = "export function go() { return 2; }\n";
  await s.flush();
  const changed = await loadFixture(s.root);
  assert.equal(expectedFreshness(base, changed), "stale");
  assert.equal(
    checkFreshness({ ...base, freshness: "stale" }, changed),
    "stale",
  );
  assert.throws(() => checkFreshness(historical, changed), {
    assertion: "FRESHNESS.LABEL",
  });
});
test("FRESHNESS.COMPONENTS requested producer, manifest, toolchain and config cannot borrow capture", async (t) => {
  const { s, base } = await prepared(t);
  s.fixture.comparison.producers = [];
  await s.flush();
  assert.equal(
    expectedFreshness(base, await loadFixture(s.root)),
    "possiblyStale",
  );
  s.fixture.comparison.producers = structuredClone(s.fixture.producers);
  s.fixture.comparison.producers[1].version = "2";
  await s.flush();
  assert.equal(
    expectedFreshness(base, await loadFixture(s.root)),
    "possiblyStale",
  );
  s.fixture.comparison.producers[1].version = "1";
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    id: "r2",
    documents: [{ key: s.document, revisionId: "r2", sourceFile: "r2/go.js" }],
  });
  s.files["r2/go.js"] = s.source;
  s.fixture.comparison.revisionId = "r2";
  for (const field of ["configHash", "dependencyHash", "toolchainHash"]) {
    const old = s.fixture.revisions[0][field];
    const kind =
      field === "dependencyHash"
        ? "dependency"
        : field.slice(0, -4).toLowerCase();
    const file = `captures/updated-${field}.txt`,
      bytes = `updated-${field}`;
    s.files[file] = bytes;
    s.fixture.captures.push({
      ref: `updated-${field}`,
      kind,
      file,
      hash: hash(bytes),
    });
    s.fixture.revisions[1][field] = hash(bytes);
    await s.flush();
    assert.equal(
      expectedFreshness(base, await loadFixture(s.root)),
      "possiblyStale",
      field,
    );
    s.fixture.revisions[1][field] = old;
  }
});
test("TARGET_STALENESS.TWO_REVISION_CONTEXT evaluates at the binding revision, not the r2 comparison", async (t) => {
  const { s, r1, base } = await prepared(t);
  const target = {
    sourceSetId: "main",
    language: "javascript",
    path: "src/target.js",
  };
  s.files["src/target.js"] = "export const target = 1;\n";
  s.fixture.revisions[0].documents.push({
    key: target,
    revisionId: "r1",
    sourceFile: "src/target.js",
  });
  addTargetDeclaration(s, target, "r1", "src/target.js");
  await s.flush();
  const snapshot = await loadFixture(s.root);
  const id = [
    ...verifiedDeclarations(snapshot)
      .get(JSON.stringify(["main", "r1"]))
      .keys(),
  ][0];
  let declarations = verifiedDeclarations(snapshot);
  const binding = {
    callId: null,
    join: {
      anchor: {
        document: s.document,
        revisionId: "r1",
        contentHash: base.contentHash,
        range: { start: 0, end: 1 },
        kind: "invocation",
      },
      status: "unmatched",
      candidateIds: [],
      diagnostic: null,
    },
    resolution: "resolved",
    declaredTarget: {
      kind: "internal",
      syntaxId: id,
      document: target,
      revisionId: "r1",
    },
    candidates: [],
    dispatch: "direct",
    possibleDispatch: [],
    possibleDispatchComplete: false,
    staleTarget: false,
    provenanceId: "proof1",
  };
  assert.equal(
    checkStaleTarget(
      binding,
      {
        ...base,
        producerId: "native",
        evidenceKind: "measuredSyntax",
        basis: null,
      },
      snapshot,
      declarations,
    ),
    false,
  );
  const proof = {
    ...base,
    producerId: "native",
    evidenceKind: "measuredSyntax",
    basis: null,
  };
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    id: "r2",
    documents: s.fixture.revisions[0].documents.map((x) => ({
      ...x,
      revisionId: "r2",
      sourceFile: x.key.path === "src/target.js" ? "r2/target.js" : "r2/go.js",
    })),
  });
  s.files["r2/go.js"] = s.source;
  s.files["r2/target.js"] = s.files["src/target.js"];
  s.fixture.comparison.revisionId = "r2";
  addTargetDeclaration(s, target, "r2", "r2/target.js");
  await s.flush();
  declarations = verifiedDeclarations(await loadFixture(s.root));
  assert.equal(
    expectedStaleTarget(
      binding,
      proof,
      await loadFixture(s.root),
      declarations,
    ),
    false,
  );
  s.files["r2/target.js"] = "export const target = 2;\n";
  await s.flush();
  const changed = await loadFixture(s.root);
  // v1 (#52): the r1 binding's r1 target is evaluated at r1, so a changed r2
  // comparison never makes it stale, and a claimed true bit is rejected.
  assert.equal(
    expectedStaleTarget(binding, proof, changed, declarations),
    false,
  );
  assert.equal(checkStaleTarget(binding, proof, changed, declarations), false);
  assert.throws(
    () =>
      checkStaleTarget(
        { ...binding, staleTarget: true },
        proof,
        changed,
        declarations,
      ),
    { assertion: "TARGET_STALENESS.LABEL", field: "staleTarget" },
  );
  // A target at another revision than its binding is invalid in v1.
  assert.throws(
    () =>
      expectedStaleTarget(
        {
          ...binding,
          declaredTarget: { ...binding.declaredTarget, revisionId: "r2" },
        },
        proof,
        changed,
        declarations,
      ),
    { assertion: "TARGET_STALENESS.REVISION", field: "declaredTarget" },
  );
});

test("FRESHNESS.REQUESTED_DESCRIPTOR compares independently without requiring requested executable", async (t) => {
  const { s, base } = await prepared(t);
  for (const mutate of [
    (x) => (x.version = "changed"),
    (x) => (x.executableHash = "f".repeat(64)),
    (x) => (x.positionEncoding = "utf16"),
    (x) => (x.languages = ["javascript", "rust"]),
    (x) => (x.languages = ["rust"]),
  ]) {
    s.fixture.comparison.producers = structuredClone(s.fixture.producers);
    mutate(s.fixture.comparison.producers[1]);
    await s.flush();
    const loaded = await loadFixture(s.root);
    assert.equal(expectedFreshness(base, loaded), "possiblyStale");
    assert.equal(
      checkFreshness({ ...base, freshness: "possiblyStale" }, loaded),
      "possiblyStale",
    );
    assert.throws(() => checkFreshness(base, loaded), {
      assertion: "FRESHNESS.LABEL",
    });
  }
  s.fixture.comparison.producers = [];
  await s.flush();
  assert.equal(
    expectedFreshness(base, await loadFixture(s.root)),
    "possiblyStale",
  );
});
test("FRESHNESS.SOURCE_SET treats identical caller bytes on another admitted set as possiblyStale", async (t) => {
  const { s, base } = await prepared(t);
  s.fixture.sourceSets.push({ ...s.fixture.sourceSets[0], id: "other" });
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    sourceSetId: "other",
    documents: [
      {
        key: { ...s.document, sourceSetId: "other" },
        revisionId: "r1",
        sourceFile: "other/go.js",
      },
    ],
  });
  s.files["other/go.js"] = s.source;
  s.fixture.comparison.sourceSetId = "other";
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.equal(expectedFreshness(base, loaded), "possiblyStale");
  assert.equal(
    checkFreshness({ ...base, freshness: "possiblyStale" }, loaded),
    "possiblyStale",
  );
});
test("BASIS.RAW_FACT rejects missing and contradictory captured semantic proof", async (t) => {
  const { s, base } = await prepared(t);
  const artifact = JSON.parse(s.files["captures/fact.json"]);
  artifact.facts = [];
  s.files["captures/fact.json"] = JSON.stringify(artifact);
  s.fixture.captures.find((x) => x.ref === "fact").hash = hash(
    s.files["captures/fact.json"],
  );
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.SEMANTIC" });
  const other = await prepared(t);
  assert.throws(
    () => checkCapturedBasis({ ...other.base, id: "fabricated" }, other.r1),
    { assertion: "BASIS.RAW_FACT" },
  );
});
test("TARGET_STALENESS verifies the captured declaration; later r2 changes never mark the r1 binding stale", async (t) => {
  const { s, base } = await prepared(t);
  const target = {
    sourceSetId: "main",
    language: "javascript",
    path: "src/target.js",
  };
  s.files["src/target.js"] = "export const target = 1;\n";
  s.fixture.revisions[0].documents.push({
    key: target,
    revisionId: "r1",
    sourceFile: "src/target.js",
  });
  addTargetDeclaration(s, target, "r1", "src/target.js");
  await s.flush();
  const loaded = await loadFixture(s.root);
  const id = [
    ...verifiedDeclarations(loaded)
      .get(JSON.stringify(["main", "r1"]))
      .keys(),
  ][0];
  const proof = {
    ...base,
    producerId: "native",
    evidenceKind: "measuredSyntax",
    basis: null,
  };
  const binding = {
    callId: null,
    join: {
      anchor: {
        document: s.document,
        revisionId: "r1",
        contentHash: base.contentHash,
        range: { start: 0, end: 1 },
        kind: "invocation",
      },
      status: "unmatched",
      candidateIds: [],
      diagnostic: null,
    },
    resolution: "resolved",
    declaredTarget: {
      kind: "internal",
      syntaxId: id,
      document: target,
      revisionId: "r1",
    },
    candidates: [],
    dispatch: "direct",
    possibleDispatch: [],
    possibleDispatchComplete: false,
    staleTarget: false,
    provenanceId: "proof1",
  };
  const declarations = verifiedDeclarations(loaded);
  assert.equal(
    expectedStaleTarget(binding, proof, loaded, declarations),
    false,
  );
  assert.throws(() => expectedStaleTarget(binding, proof, loaded), {
    assertion: "TARGET_STALENESS.DECLARATIONS",
  });
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    id: "r2",
    documents: s.fixture.revisions[0].documents.map((x) => ({
      ...x,
      revisionId: "r2",
      sourceFile: x.key.path === "src/target.js" ? "r2/target.js" : "r2/go.js",
    })),
  });
  s.files["r2/go.js"] = s.source;
  s.files["r2/target.js"] = s.files["src/target.js"];
  s.fixture.comparison.revisionId = "r2";
  addTargetDeclaration(s, target, "r2", "r2/target.js");
  await s.flush();
  const selected = await loadFixture(s.root),
    both = verifiedDeclarations(selected);
  assert.equal(expectedStaleTarget(binding, proof, selected, both), false);
  const native = JSON.parse(s.files["captures/native.json"]);
  native.declarations = native.declarations.filter(
    (x) => x.ref !== "target-r2",
  );
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const absent = await loadFixture(s.root);
  assert.equal(
    absent.selected.documents.find((x) => x.key.path === target.path)
      .contentHash,
    selected.selected.documents.find((x) => x.key.path === target.path)
      .contentHash,
  );
  // v1 (#52): the r1 target is present at r1, so a missing r2 declaration
  // does not make it stale, and a claimed true bit is rejected.
  assert.equal(
    expectedStaleTarget(binding, proof, absent, verifiedDeclarations(absent)),
    false,
  );
  assert.throws(
    () =>
      checkStaleTarget(
        { ...binding, staleTarget: true },
        proof,
        absent,
        verifiedDeclarations(absent),
      ),
    { assertion: "TARGET_STALENESS.LABEL" },
  );
  addTargetDeclaration(s, target, "r2", "r2/target.js");
  await s.flush();
  const withoutCaptured = new Map(both);
  withoutCaptured.set(JSON.stringify(["main", "r1"]), new Map());
  assert.throws(
    () => expectedStaleTarget(binding, proof, selected, withoutCaptured),
    { assertion: "TARGET_STALENESS.CAPTURED" },
  );
  s.fixture.revisions[1].documents.pop();
  const nativeMissing = JSON.parse(s.files["captures/native.json"]);
  nativeMissing.declarations = nativeMissing.declarations.filter(
    (x) => x.revisionId !== "r2",
  );
  s.files["captures/native.json"] = JSON.stringify(nativeMissing);
  delete s.files["r2/target.js"];
  await rm(join(s.root, "r2/target.js"));
  await s.flush();
  const missing = await loadFixture(s.root);
  assert.equal(
    expectedStaleTarget(binding, proof, missing, verifiedDeclarations(missing)),
    false,
  );
  s.fixture.revisions[1].documents.push({
    key: target,
    revisionId: "r2",
    sourceFile: "r2/target.js",
  });
  s.files["r2/target.js"] = "export const target = 2;\n";
  addTargetDeclaration(s, target, "r2", "r2/target.js");
  await s.flush();
  const changed = await loadFixture(s.root);
  assert.equal(
    expectedStaleTarget(binding, proof, changed, verifiedDeclarations(changed)),
    false,
  );
});

test("BASIS.RAW_RELATIONSHIP binds a separate relationship proof to the captured source", async (t) => {
  const { s, base } = await prepared(t);
  const native = JSON.parse(s.files["captures/native.json"]);
  const start = s.source.indexOf("go");
  native.declarations.push({
    ref: "go-declaration",
    nativeId: null,
    role: "production",
    document: s.document,
    revisionId: "r1",
    parentRef: null,
    kind: "function",
    name: "go",
    range: { encoding: "utf8", start: 0, end: Buffer.byteLength(s.source) },
    nameRange: { encoding: "utf8", start, end: start + 2 },
    header: {
      kind: "function",
      name: "go",
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases: [],
    },
    signature: null,
    witnesses: [
      {
        field: "name",
        witness: {
          range: { encoding: "utf8", start, end: start + 2 },
          text: "go",
        },
      },
    ],
  });
  s.files["captures/native.json"] = JSON.stringify(native);
  const source = {
    kind: "internal",
    declarationRef: "go-declaration",
    revisionId: "r1",
  };
  const target = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "example/base",
      scope: "global",
      document: null,
    },
  };
  const relationship = {
    kind: "typeRelationship",
    ref: "relationship1",
    relationshipKind: "extends",
    source,
    target,
    provenanceRef: "relationship-proof",
  };
  const raw = JSON.parse(s.files["captures/fact.json"]);
  raw.facts.push(relationship);
  s.files["captures/fact.json"] = JSON.stringify(raw);
  const artifactHash = hash(s.files["captures/fact.json"]);
  s.fixture.captures.find((x) => x.ref === "fact").hash = artifactHash;
  const annotation = JSON.parse(s.files["src/go.js.annotations.json"]);
  annotation.facts[0].record.basis.artifactHash = artifactHash;
  const relationshipProof = {
    ...structuredClone(annotation.facts[0].record),
    id: "relationship-proof",
    evidenceKind: "typeRelationship",
  };
  annotation.facts.push(
    {
      kind: "provenance",
      ref: "relationship-provenance",
      record: relationshipProof,
    },
    relationship,
  );
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.equal(
    checkCapturedBasis(relationshipProof, loaded).producer.kind,
    "semantic",
  );
  const altered = { ...relationshipProof, contentHash: "0".repeat(64) };
  assert.throws(() => checkCapturedBasis(altered, loaded), {
    assertion: "BASIS.CONTENT_HASH",
  });
  const sameId = { ...relationshipProof, evidenceKind: "semanticReference" };
  assert.throws(() => checkCapturedBasis(sameId, loaded), {
    assertion: "BASIS.RAW_FACT",
  });
  annotation.facts.at(-1).source = {
    ...source,
    declarationRef: "wrong-direction",
  };
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.SEMANTIC" });
  annotation.facts[annotation.facts.length - 1] = relationship;
  annotation.facts.at(-2).record = {
    ...relationshipProof,
    evidenceKind: "semanticReference",
  };
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.SEMANTIC" });
  annotation.facts.at(-2).record = relationshipProof;
  annotation.facts[annotation.facts.length - 1] = {
    ...relationship,
    provenanceRef: base.id,
  };
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.SEMANTIC" });
});
test("IDENTITY.SEMANTIC rejects normalized provenance in raw capture", async (t) => {
  const { s, base } = await prepared(t);
  const raw = JSON.parse(s.files["captures/fact.json"]);
  raw.facts.push({
    kind: "provenance",
    ref: "raw-proof",
    record: { ...base, basis: { ...base.basis, artifactHash: "0".repeat(64) } },
  });
  s.files["captures/fact.json"] = JSON.stringify(raw);
  s.fixture.captures.find((x) => x.ref === "fact").hash = hash(
    s.files["captures/fact.json"],
  );
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.SEMANTIC" });
});

test("FRESHNESS.COVERAGE keeps partial fresh, complete stale and failed refresh historical", async (t) => {
  const { s, base } = await prepared(t);
  const intent = {
    producerId: "semantic",
    document: s.document,
    revisionId: "r1",
    requestedRoles: ["call"],
    measurementSupport: [
      "declarationName",
      "callee",
      "invocation",
      "reference",
    ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
  };
  s.fixture.coverageIntents = [intent];
  intent.measurementSupport[1] = {
    kind: "callee",
    gaps: [], available: false,
    diagnostic: "partial measurement",
  };
  await s.flush();
  let loaded = await loadFixture(s.root);
  assert.equal(
    loaded.fixture.coverageIntents[0].measurementSupport[1].available,
    false,
  );
  assert.equal(checkFreshness(base, loaded), "fresh");
  intent.measurementSupport[1] = {
    kind: "callee",
    gaps: [], available: true,
    diagnostic: null,
  };
  const coverage = {
    producerId: "semantic",
    language: "javascript",
    sourceSetId: "main",
    documentPath: s.document.path,
    revisionId: "r1",
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["call"],
    observedRoles: ["call"],
    diagnostic: null,
  };
  const capturedAnnotation = JSON.parse(s.files["src/go.js.annotations.json"]);
  capturedAnnotation.facts.push({
    kind: "coverage",
    ref: "r1-coverage",
    record: coverage,
  });
  s.files["src/go.js.annotations.json"] = JSON.stringify(capturedAnnotation);
  s.fixture.revisions.push({
    ...s.fixture.revisions[0],
    id: "r2",
    documents: [{ key: s.document, revisionId: "r2", sourceFile: "r2/go.js" }],
  });
  s.files["r2/go.js"] = "export function go() { return 2; }\n";
  s.fixture.comparison.revisionId = "r2";
  await s.flush();
  loaded = await loadFixture(s.root);
  assert.ok(
    loaded.fixture.coverageIntents[0].measurementSupport.every(
      (x) => x.available,
    ),
  );
  assert.equal(
    checkFreshness({ ...base, freshness: "stale" }, loaded),
    "stale",
  );
  const refresh = { ...structuredClone(intent), revisionId: "r2" };
  s.fixture.coverageIntents.push(refresh);
  const failedCoverage = {
    ...coverage,
    revisionId: "r2",
    state: "failed",
    observedRoles: [],
    diagnostic: "semantic refresh failed",
  };
  s.files["r2/go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: s.document,
    revisionId: "r2",
    scenarios: [],
    facts: [
      { kind: "coverage", ref: "r2-failed-coverage", record: failedCoverage },
    ],
  });
  s.fixture.annotationFiles.push("r2/go.js.annotations.json");
  await s.flush();
  loaded = await loadFixture(s.root);
  assert.equal(loaded.fixture.coverageIntents[0].revisionId, "r1");
  assert.ok(
    loaded.fixture.coverageIntents[0].measurementSupport.every(
      (x) => x.available,
    ),
  );
  assert.ok(
    loaded.fixture.coverageIntents
      .find((x) => x.revisionId === "r2")
      ?.measurementSupport.every((x) => x.available),
  );
  const rows = loaded.annotations.flatMap((x) =>
    x.facts.filter((f) => f.kind === "coverage").map((f) => f.record),
  );
  assert.equal(
    JSON.stringify(rows.find((x) => x.revisionId === "r1")),
    JSON.stringify(coverage),
  );
  const failed = rows.find((x) => x.revisionId === "r2");
  assert.equal(JSON.stringify(failed), JSON.stringify(failedCoverage));
  assert.equal(failed.requested, true);
  assert.equal(failed.selected, true);
  assert.equal(failed.state, "failed");
  assert.ok(failed.diagnostic);
  const retained = loaded.annotations
    .flatMap((x) => x.facts)
    .find((x) => x.kind === "provenance" && x.record.id === base.id)?.record;
  assert.equal(JSON.stringify(retained), JSON.stringify(base));
  assert.equal(expectedFreshness(retained, loaded), "stale");
  assert.equal(
    checkFreshness({ ...retained, freshness: "stale" }, loaded),
    "stale",
  );
  assert.throws(
    () => checkFreshness({ ...retained, freshness: "fresh" }, loaded),
    { assertion: "FRESHNESS.LABEL", field: "freshness" },
  );
});
