import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { loadFixture } from "../load.mjs";
import {
  normalize,
  normalizeRelationship,
  buildAnchorIndex,
  joinAnchor,
} from "../normalize.mjs";
import { contentHash } from "../identity.mjs";
import { registerControls, runControl } from "./mutations.mjs";
const hash = (x) => contentHash(Buffer.from(x));
async function specimen() {
  const root = await mkdtemp(join(tmpdir(), "admission-"));
  const files = {};
  const put = (path, value) => {
    files[path] = typeof value === "string" ? value : JSON.stringify(value);
  };
  const source = "export function go() { return 1; }\n";
  const document = {
    sourceSetId: "main",
    language: "javascript",
    path: "src/go.js",
  };
  const native = {
    id: "native",
    version: "1",
    executableHash: hash("native 1"),
    kind: "native",
    languages: ["javascript"],
    positionEncoding: "utf8",
  };
  const semantic = {
    ...native,
    id: "semantic",
    kind: "semantic",
    executableHash: hash("semantic 1"),
  };
  const captures = [];
  for (const [ref, kind, file, text] of [
    ["native", "executable", "captures/native.bin", "native 1"],
    ["semantic", "executable", "captures/semantic.bin", "semantic 1"],
    ["tool", "toolchain", "captures/tool.txt", "tool"],
    ["config", "config", "captures/config.txt", "config"],
    ["deps", "dependency", "captures/deps.txt", "deps"],
    [
      "fact",
      "semanticArtifact",
      "captures/fact.json",
      JSON.stringify({ formatVersion: 1, producerId: "semantic", facts: [] }),
    ],
  ]) {
    put(file, text);
    captures.push({ ref, kind, file, hash: hash(text) });
  }
  put("captures/native.json", {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [],
    calls: [],
    controls: [],
    references: [],
  });
  put("src/go.js", source);
  put("src/go.js.annotations.json", {
    formatVersion: 1,
    document,
    revisionId: "r1",
    scenarios: [],
    facts: [],
  });
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  const fixture = {
    formatVersion: 1,
    profile: "example",
    language: "javascript",
    sourceSets: [
      {
        id: "main",
        rootId: "root",
        languages: ["javascript"],
        dependencies: [],
      },
    ],
    producers: [native, semantic],
    revisions: [
      {
        id: "r1",
        sourceSetId: "main",
        documents: [
          { key: document, revisionId: "r1", sourceFile: "src/go.js" },
        ],
        toolchainHash: hash("tool"),
        configHash: hash("config"),
        dependencyHash: hash("deps"),
      },
    ],
    comparison: {
      sourceSetId: "main",
      revisionId: "r1",
      producers: [native, semantic],
    },
    coverageIntents: [],
    nativeArtifact: "captures/native.json",
    semanticArtifacts: ["captures/fact.json"],
    annotationFiles: ["src/go.js.annotations.json"],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  async function flush() {
    put("fixture.json", fixture);
    for (const [path, value] of Object.entries(files)) {
      await mkdir(dirname(join(root, path)), { recursive: true });
      await writeFile(join(root, path), value);
    }
  }
  await flush();
  return {
    root,
    files,
    fixture,
    document,
    source,
    flush,
    cleanup: () => rm(root, { recursive: true, force: true }),
  };
}
const span = (start, end) => ({ encoding: "utf8", start, end });
const row = (s, ref = "decl") => ({
  ref,
  nativeId: "opaque",
    role: "production",
  document: s.document,
  revisionId: "r1",
  parentRef: null,
  kind: "function",
  name: "go",
  range: span(0, 31),
  nameRange: span(16, 18),
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
  witnesses: ["name", "header.name"].map((field) => ({
    field,
    witness: { range: span(16, 18), text: "go" },
  })),
});
function setNative(s, rows) {
  s.files["captures/native.json"] = JSON.stringify({
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: rows,
    calls: [],
    controls: [],
    references: [],
  });
}
test("NORMALIZE.SOURCE identity and native diagnostic independence", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  setNative(s, [row(s)]);
  await s.flush();
  let result = normalize(await loadFixture(s.root));
  assert.equal(result.records.declarations.length, 1);
  assert.deepEqual(result.records.declarations[0].nameRange, {
    start: 16,
    end: 18,
  });
  const first = result.records.declarations[0].syntaxId;
  setNative(s, [{ ...row(s), nativeId: "changed" }]);
  await s.flush();
  result = normalize(await loadFixture(s.root));
  assert.equal(result.records.declarations[0].syntaxId, first);
  assert.equal(result.identityMap.get("decl"), first);
});
test("NORMALIZE.DUPLICATE owner/kind/range rejected", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  setNative(s, [row(s), { ...row(s, "other"), nativeId: null }]);
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.throws(() => normalize(loaded), /IDENTITY.ORDINAL/);
});

test("NORMALIZE.OCCURRENCE source-derived call/reference ordinals and exact tuple joins", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const native = {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [row(s)],
    calls: [
      {
        ref: "call",
        nativeId: "ignored",
        document: s.document,
        revisionId: "r1",
        ownerRef: "decl",
        range: span(16, 20),
        calleeRange: span(16, 18),
        spelling: "go",
        regionRefs: [],
        witnesses: [
          { field: "spelling", witness: { range: span(16, 18), text: "go" } },
        ],
      },
    ],
    controls: [],
    references: [
      {
        ref: "reference",
        nativeId: null,
        document: s.document,
        revisionId: "r1",
        ownerRef: "decl",
        range: span(16, 18),
        spelling: "go",
        witnesses: [
          { field: "spelling", witness: { range: span(16, 18), text: "go" } },
        ],
      },
    ],
  };
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const loaded = await loadFixture(s.root);
  const result = normalize(loaded);
  assert.equal(result.records.calls.length, 1);
  assert.match(result.records.calls[0].id, /^occ:v2:/);
  assert.equal(result.records.references.length, 0);
  assert.equal(result.identityMap.has("reference"), true);
});
test("NORMALIZE.CALLEE does not guess a range from the separate spelling witness", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const call = {
    ref: "call",
    nativeId: null,
    document: s.document,
    revisionId: "r1",
    ownerRef: "decl",
    range: span(16, 20),
    calleeRange: null,
    spelling: "go",
    regionRefs: [],
    witnesses: [
      { field: "spelling", witness: { range: span(16, 18), text: "go" } },
    ],
  };
  const native = {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [row(s)],
    calls: [call],
    controls: [],
    references: [],
  };
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.equal(normalize(loaded).records.calls[0].calleeRange, null);
  call.witnesses = [];
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  assert.throws(() => normalize({ ...loaded, native }), /WITNESS.MISSING/);
});

test("NORMALIZE.REFERENCE_JOIN non-exact diagnostics do not fabricate reference records", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const sourceHash = hash(s.source);
  const basis = {
    producerId: "semantic",
    producerVersion: "1",
    producerHash: s.fixture.producers[1].executableHash,
    artifactHash: null,
    language: "javascript",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: null,
    toolchainHash: s.fixture.revisions[0].toolchainHash,
    configHash: s.fixture.revisions[0].configHash,
    dependencyHash: s.fixture.revisions[0].dependencyHash,
    lookupDependencies: [],
  };
  const anchor = {
    document: s.document,
    revisionId: "r1",
    contentHash: sourceHash,
    kind: "reference",
    range: span(16, 18),
    ownerRef: "decl",
  };
  const fact = {
    kind: "reference",
    ref: "ref1",
    anchor,
    record: {
      site: "use",
      roles: ["read"],
      resolution: "unresolved",
      declaredTarget: null,
      candidates: [],
      provenanceId: "proof1",
    },
  };
  const native = {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [row(s)],
    calls: [],
    controls: [],
    references: [],
  };
  s.files["captures/native.json"] = JSON.stringify(native);
  const raw = JSON.stringify({
    formatVersion: 1,
    producerId: "semantic",
    facts: [fact],
  });
  s.files["captures/fact.json"] = raw;
  s.fixture.captures.find((x) => x.ref === "fact").hash = hash(raw);
  await s.flush();
  let loaded = await loadFixture(s.root);
  basis.artifactHash = hash(raw);
  basis.sourceManifestHash = loaded.sourceManifestHash(loaded.selected);
  const provenance = {
    id: "proof1",
    producerId: "semantic",
    document: s.document,
    revisionId: "r1",
    contentHash: sourceHash,
    evidenceKind: "semanticReference",
    basis,
    freshness: "fresh",
  };
  s.files["src/go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: s.document,
    revisionId: "r1",
    scenarios: [],
    facts: [
      { kind: "provenance", ref: "provenance1", record: provenance },
      fact,
    ],
  });
  await s.flush();
  loaded = await loadFixture(s.root);
  const output = normalize(loaded).records;
  assert.equal(output.references.length, 0);
  assert.equal(output.referenceJoinDiagnostics.length, 1);
  assert.deepEqual(output.referenceJoinDiagnostics[0].join.candidateIds, []);
  assert.equal(output.referenceJoinDiagnostics[0].join.status, "unmatched");
  native.references.push({
    ref: "native-reference",
    nativeId: null,
    document: s.document,
    revisionId: "r1",
    ownerRef: "decl",
    range: span(16, 18),
    spelling: "go",
    witnesses: [
      { field: "spelling", witness: { range: span(16, 18), text: "go" } },
    ],
  });
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const exact = normalize(await loadFixture(s.root)).records;
  assert.equal(exact.referenceJoinDiagnostics.length, 0);
  assert.equal(exact.references.length, 1);
  assert.equal(exact.references[0].spelling, "go");
  assert.equal(exact.references[0].resolution, "unresolved");
});

test("NORMALIZE.RELATIONSHIP maps all authored kinds with directed internal source", () => {
  const document = {
    sourceSetId: "main",
    language: "javascript",
    path: "src/go.js",
  };
  const declarations = new Map([
    ["child", { document, revisionId: "r1" }],
    ["parent", { document, revisionId: "r1" }],
  ]);
  const ids = new Map([
    ["child", "sid:v1:" + "a".repeat(32)],
    ["parent", "sid:v1:" + "b".repeat(32)],
  ]);
  for (const kind of ["extends", "implements", "overrides"]) {
    const fact = {
      kind: "typeRelationship",
      ref: kind,
      relationshipKind: kind,
      source: { kind: "internal", declarationRef: "child", revisionId: "r1" },
      target: { kind: "internal", declarationRef: "parent", revisionId: "r1" },
      provenanceRef: "independent-proof",
    };
    const output = normalizeRelationship(fact, ids, declarations);
    assert.equal(output.kind, kind);
    assert.equal(output.source.syntaxId, ids.get("child"));
    assert.equal(output.target.syntaxId, ids.get("parent"));
    assert.equal(output.provenanceId, "independent-proof");
  }
});

test("NORMALIZE.JOIN counts distinct measured candidate IDs and exact tuple", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  setNative(s, [row(s)]);
  await s.flush();
  const loaded = await loadFixture(s.root);
  const anchor = {
    document: s.document,
    revisionId: "r1",
    contentHash: contentHash(Buffer.from(s.source)),
    kind: "reference",
    range: span(16, 18),
    ownerRef: "decl",
  };
  const measured = {
    document: s.document,
    revisionId: "r1",
    contentHash: anchor.contentHash,
    kind: "reference",
    range: { start: 16, end: 18 },
  };
  const one = {
      ref: "one",
      id: "occ:v2:" + "1".repeat(32),
      anchor: measured,
      ownerRef: "decl",
    },
    two = { ...one, ref: "two", id: "occ:v2:" + "2".repeat(32) };
  let join = joinAnchor(anchor, buildAnchorIndex([one, one]), loaded);
  assert.equal(join.status, "exact");
  join = joinAnchor(anchor, buildAnchorIndex([one, two]), loaded);
  assert.equal(join.status, "ambiguous");
  assert.equal(join.candidateIds.length, 2);
  join = joinAnchor(
    { ...anchor, range: span(17, 18) },
    buildAnchorIndex([one]),
    loaded,
  );
  assert.equal(join.status, "unmatched");
});

// Every mutation is applied to admitted source-derived rows, never to a mock ID.
function measured(s) {
  return {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [row(s)],
    calls: [
      {
        ref: "call",
        nativeId: null,
        document: s.document,
        revisionId: "r1",
        ownerRef: "decl",
        range: span(16, 20),
        calleeRange: span(16, 18),
        spelling: "go",
        regionRefs: [],
        witnesses: [
          { field: "spelling", witness: { range: span(16, 18), text: "go" } },
        ],
      },
    ],
    controls: [],
    references: [
      {
        ref: "reference",
        nativeId: null,
        document: s.document,
        revisionId: "r1",
        ownerRef: "decl",
        range: span(16, 18),
        spelling: "go",
        witnesses: [
          { field: "spelling", witness: { range: span(16, 18), text: "go" } },
        ],
      },
    ],
  };
}
async function loadedMeasured(t) {
  const s = await specimen();
  t.after(s.cleanup);
  s.files["captures/native.json"] = JSON.stringify(measured(s));
  await s.flush();
  return { s, loaded: await loadFixture(s.root) };
}
async function invalidMutation(t, mutate, pattern) {
  const { loaded } = await loadedMeasured(t);
  const before = normalize(loaded);
  assert.equal(before.records.calls.length, 1);
  assert.equal(before.records.declarations.length, 1);
  assert.equal(before.records.provenance.length, 3);
  mutate(loaded.native, loaded);
  assert.throws(() => normalize(loaded), pattern);
}
for (const [name, mutation, error] of [
  [
    "missing header name",
    (n) => n.declarations[0].witnesses.pop(),
    /WITNESS.MISSING/,
  ],
  [
    "duplicate witness",
    (n) =>
      n.declarations[0].witnesses.push(
        structuredClone(n.declarations[0].witnesses[0]),
      ),
    /NORMALIZE.WITNESS/,
  ],
  [
    "changed leaf value",
    (n) => (n.declarations[0].witnesses[0].witness.text = "no"),
    /NORMALIZE.WITNESS|WITNESS.BYTES/,
  ],
  [
    "name span outside declaration",
    (n) => (n.declarations[0].range = span(0, 17)),
    /NORMALIZE.NAME/,
  ],
  [
    "header mismatch",
    (n) => (n.declarations[0].header.name = "notGo"),
    /NORMALIZE.HEADER/,
  ],
  [
    "anonymous name",
    (n) => (n.declarations[0].kind = "anonymousFunction"),
    /NORMALIZE.NAME/,
  ],
  [
    "missing call spelling",
    (n) => (n.calls[0].witnesses = []),
    /WITNESS.MISSING/,
  ],
  [
    "callee mismatch",
    (n) => (n.calls[0].calleeRange = span(17, 19)),
    /NORMALIZE.WITNESS/,
  ],
  [
    "call outside owner",
    (n) => (n.calls[0].range = span(30, 33)),
    /NORMALIZE.OWNER/,
  ],
  [
    "missing reference spelling",
    (n) => (n.references[0].witnesses = []),
    /WITNESS.MISSING/,
  ],
  [
    "reference bytes differ",
    (n) => (n.references[0].spelling = "no"),
    /NORMALIZE.WITNESS|WITNESS.BYTES/,
  ],
  [
    "bad owner",
    (n) => (n.references[0].ownerRef = "missing"),
    /NORMALIZE.OWNER/,
  ],
  [
    "duplicate call location",
    (n) => n.calls.push({ ...structuredClone(n.calls[0]), ref: "call2" }),
    /IDENTITY.OCCURRENCE/,
  ],
  [
    "duplicate reference location",
    (n) =>
      n.references.push({ ...structuredClone(n.references[0]), ref: "ref2" }),
    /IDENTITY.OCCURRENCE/,
  ],
])
  test(`measured mutation: ${name}`, async (t) =>
    invalidMutation(t, mutation, error));
test("source leaves include parameter, signature, modifiers, bases, and result type", async (t) => {
  const { loaded } = await loadedMeasured(t),
    decl = loaded.native.declarations[0];
  const source = loaded.sources.get(
    JSON.stringify(["main", "r1", decl.document.path]),
  );
  const name = "go";
  const paths = [
    "header.modifiers[0]",
    "header.typeParameters[0]",
    "header.parameters[0].name",
    "header.parameters[0].type",
    "header.resultType",
    "header.bases[0]",
    "signature.parameterTypes[0]",
  ];
  decl.header = {
    ...decl.header,
    modifiers: [name],
    typeParameters: [name],
    parameters: [{ name, type: name, variadic: false }],
    resultType: name,
    bases: [name],
  };
  decl.signature = {
    parameterTypes: [name],
    typeParameterCount: 1,
    variadic: false,
  };
  for (const field of paths)
    decl.witnesses.push({
      field,
      witness: { range: span(16, 18), text: name },
    });
  assert.equal(
    normalize(loaded).records.declarations[0].key.signature.parameterTypes[0],
    name,
  );
  for (const field of paths) {
    const changed = { ...loaded, native: structuredClone(loaded.native) };
    changed.native.declarations[0].witnesses = decl.witnesses.filter(
      (x) => x.field !== field,
    );
    assert.throws(() => normalize(changed), /WITNESS.MISSING/);
  }
  decl.signature.typeParameterCount = 0;
  assert.throws(() => normalize(loaded), /NORMALIZE.SIGNATURE/);
  assert.equal(source.subarray(16, 18).toString(), "go");
});
test("source identity and ordering are deterministic under shuffled rows and nativeId changes", async (t) => {
  const { loaded } = await loadedMeasured(t);
  const before = normalize(loaded);
  for (const group of ["declarations", "calls", "references"])
    loaded.native[group].reverse().forEach((x) => (x.nativeId = "different"));
  const after = normalize(loaded);
  assert.deepEqual(after.records, before.records);
  assert.deepEqual([...after.identityMap], [...before.identityMap]);
});
test("control chain requires unique ordered containing regions", async (t) => {
  const { loaded } = await loadedMeasured(t),
    n = loaded.native;
  const base = {
    nativeId: null,
    document: n.declarations[0].document,
    revisionId: "r1",
    ownerRef: "decl",
    arm: null,
    witnesses: [],
  };
  n.controls = [
    {
      ...base,
      ref: "outer",
      parentRef: null,
      kind: "block",
      range: span(0, 30),
    },
    {
      ...base,
      ref: "inner",
      parentRef: "outer",
      kind: "if",
      range: span(15, 22),
    },
  ];
  n.calls[0].regionRefs = ["outer", "inner"];
  assert.equal(normalize(loaded).records.calls[0].regionIds.length, 2);
  n.calls[0].regionRefs = ["inner", "outer"];
  assert.throws(() => normalize(loaded), /NORMALIZE.REGION/);
  n.calls[0].regionRefs = ["outer", "outer"];
  assert.throws(() => normalize(loaded), /NORMALIZE.REGION/);
});

// Keep the invocation evidence independent of the declaration-name witnesses.
async function invocationFixture(t) {
  const s = await specimen();
  t.after(s.cleanup);
  s.source = "function main() { target(); }\nfunction target() {}\n";
  s.files["src/go.js"] = s.source;
  const declaration = (ref, name, start, end, nameStart) => ({
    ref,
    nativeId: null,
    role: "production",
    document: s.document,
    revisionId: "r1",
    parentRef: null,
    kind: "function",
    name,
    range: span(start, end),
    nameRange: span(nameStart, nameStart + name.length),
    header: {
      kind: "function",
      name,
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases: [],
    },
    signature: null,
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(nameStart, nameStart + name.length), text: name },
    })),
  });
  const native = {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations: [
      declaration("main", "main", 0, 29, 9),
      declaration("target", "target", 30, 50, 39),
    ],
    calls: [
      {
        ref: "invocation",
        nativeId: null,
        document: s.document,
        revisionId: "r1",
        ownerRef: "main",
        range: span(18, 27),
        calleeRange: span(18, 24),
        spelling: null,
        regionRefs: [],
        witnesses: [],
      },
    ],
    controls: [],
    references: [],
  };
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  return { loaded: await loadFixture(s.root), native };
}
const invocationWitness = () => ({
  field: "spelling",
  witness: { range: span(18, 24), text: "target" },
});
test("real measured target() retains all four independent nullable spelling/callee combinations", async (t) => {
  const { loaded, native } = await invocationFixture(t);
  for (const [calleeRange, spelling, witnesses] of [
    [span(18, 24), null, []],
    [null, "target", [invocationWitness()]],
    [span(18, 24), "target", [invocationWitness()]],
    [null, null, []],
  ]) {
    const input = structuredClone(native);
    Object.assign(input.calls[0], { calleeRange, spelling, witnesses });
    const output = normalize({ ...loaded, native: input });
    assert.equal(output.records.calls.length, 1);
    assert.deepEqual(
      output.records.calls[0].calleeRange,
      calleeRange === null ? null : { start: 18, end: 24 },
    );
    assert.equal(output.records.calls[0].spelling, spelling);
    assert.equal(output.recordMap.get("invocation").spelling, spelling);
    assert.equal(output.records.declarations.length, 2);
  }
});

test("nullable call spelling controls use admitted source and exact logical failures", async (t) => {
  const { loaded, native } = await invocationFixture(t);
  const withWitness = () => {
    const input = structuredClone(native);
    Object.assign(input.calls[0], {
      calleeRange: null,
      spelling: "target",
      witnesses: [invocationWitness()],
    });
    return input;
  };
  const rows = registerControls([
    {
      id: "F2.spelling-witness-missing",
      baseline: withWitness,
      check: (input) => normalize({ ...loaded, native: input }),
      mutate: (input) => {
        input.calls[0].witnesses = [];
        return input;
      },
      expectedAssertion: "WITNESS.MISSING",
      expectedCode: "invalidRecord",
      expectedField: "spelling",
    },
    {
      id: "F2.authored-spelling-changed",
      baseline: withWitness,
      check: (input) => normalize({ ...loaded, native: input }),
      mutate: (input) => {
        input.calls[0].spelling = "different";
        return input;
      },
      expectedAssertion: "NORMALIZE.WITNESS",
      expectedCode: "invalidRecord",
      expectedField: "spelling",
    },
    {
      id: "F2.spelling-witness-encoding",
      baseline: withWitness,
      check: (input) => normalize({ ...loaded, native: input }),
      mutate: (input) => {
        input.calls[0].witnesses[0].witness.range.encoding = "utf16";
        return input;
      },
      expectedAssertion: "NORMALIZE.ENCODING",
      expectedCode: "invalidRecord",
      expectedField: "witnesses.spelling",
    },
    {
      id: "F2.callee-outside-invocation",
      baseline: () => structuredClone(native),
      check: (input) => normalize({ ...loaded, native: input }),
      mutate: (input) => {
        input.calls[0].calleeRange = span(39, 45);
        return input;
      },
      expectedAssertion: "NORMALIZE.CALLEE",
      expectedCode: "invalidRecord",
      expectedField: "calleeRange",
    },
  ]);
  for (const row of rows) await t.test(row.id, () => runControl(row));
  const outside = withWitness();
  outside.calls[0].witnesses[0].witness.range = span(39, 45);
  assert.throws(
    () => normalize({ ...loaded, native: outside }),
    (error) =>
      error instanceof RangeError && /WITNESS.CONTAINMENT/.test(error.message),
  );
  const changed = withWitness();
  changed.calls[0].witnesses[0].witness.range = span(18, 23);
  assert.throws(
    () => normalize({ ...loaded, native: changed }),
    (error) => /WITNESS.BYTES/.test(error.message),
  );
});
async function semanticFixture(t, facts, proofKinds) {
  const s = await specimen();
  t.after(s.cleanup);
  s.files["captures/native.json"] = JSON.stringify(measured(s));
  const artifact = JSON.stringify({
    formatVersion: 1,
    producerId: "semantic",
    facts,
  });
  s.files["captures/fact.json"] = artifact;
  s.fixture.captures.find((x) => x.ref === "fact").hash = hash(artifact);
  await s.flush();
  const first = await loadFixture(s.root),
    producer = s.fixture.producers[1],
    revision = s.fixture.revisions[0];
  const basis = {
    producerId: "semantic",
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    artifactHash: hash(artifact),
    language: "javascript",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: first.sourceManifestHash(first.selected),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const proofs = Object.entries(proofKinds).map(([id, evidenceKind]) => ({
    kind: "provenance",
    ref: `p-${id}`,
    record: {
      id,
      producerId: "semantic",
      document: s.document,
      revisionId: "r1",
      contentHash: hash(s.source),
      evidenceKind,
      basis,
      freshness: "fresh",
    },
  }));
  s.files["src/go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: s.document,
    revisionId: "r1",
    scenarios: [],
    facts: [...proofs, ...facts],
  });
  await s.flush();
  return { s, loaded: await loadFixture(s.root) };
}
const symbol = (name = "go") => ({
  scheme: "scip",
  symbol: name,
  scope: "global",
  document: null,
});
const internal = (ref = "decl") => ({
  kind: "internal",
  declarationRef: ref,
  revisionId: "r1",
});
const anchor = (s, kind, range = span(16, 18)) => ({
  document: s.document,
  revisionId: "r1",
  contentHash: hash(s.source),
  kind,
  range,
  ownerRef: "decl",
});
test("semantic proof, symbol, declaration binding, relationship, reference, call and maps", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    h = hash("export function go() { return 1; }\n");
  const at = (kind, range = span(16, 18)) => ({
    document: d,
    revisionId: "r1",
    contentHash: h,
    kind,
    range,
    ownerRef: "decl",
  });
  const facts = [
    {
      kind: "symbol",
      ref: "sym",
      record: {
        key: symbol(),
        displayName: "go",
        declarations: [internal()],
        provenanceId: "psym",
      },
    },
    {
      kind: "declarationBinding",
      ref: "db",
      anchor: at("declarationName"),
      record: { symbols: [symbol()], provenanceId: "pdb" },
    },
    {
      kind: "typeRelationship",
      ref: "rel",
      relationshipKind: "extends",
      source: internal(),
      target: internal(),
      provenanceRef: "pr",
    },
    {
      kind: "reference",
      ref: "ref",
      anchor: at("reference"),
      record: {
        site: "use",
        roles: ["read"],
        resolution: "resolved",
        declaredTarget: internal(),
        candidates: [],
        provenanceId: "pref",
      },
    },
    {
      kind: "callBinding",
      ref: "cb",
      anchor: at("callee"),
      record: {
        resolution: "resolved",
        declaredTarget: internal(),
        candidates: [],
        dispatch: "direct",
        possibleDispatch: [],
        possibleDispatchComplete: false,
        provenanceId: "pc",
      },
    },
  ];
  const { loaded } = await semanticFixture(t, facts, {
    psym: "declarationBinding",
    pdb: "declarationBinding",
    pr: "typeRelationship",
    pref: "semanticReference",
    pc: "semanticReference",
  });
  loaded.native.controls.push({
    ref: "control",
    nativeId: null,
    document: loaded.native.declarations[0].document,
    revisionId: "r1",
    ownerRef: "decl",
    parentRef: null,
    kind: "block",
    range: span(0, 30),
    arm: null,
    witnesses: [],
  });
  loaded.native.calls[0].regionRefs = ["control"];
  loaded.annotations[0].facts.push({
    kind: "coverage",
    ref: "coverage-baseline",
    record: {
      producerId: "native",
      language: "javascript",
      sourceSetId: "main",
      documentPath: "src/go.js",
      revisionId: "r1",
      requested: true,
      selected: true,
      state: "complete",
      supportedRoles: ["read"],
      observedRoles: ["read"],
      diagnostic: null,
    },
  });
  loaded.fixture.coverageIntents.push({
    producerId: "native",
    document: loaded.native.declarations[0].document,
    revisionId: "r1",
    requestedRoles: ["read"],
    measurementSupport: [
      "declarationName",
      "callee",
      "invocation",
      "reference",
    ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
  });
  loaded.anchors.cases.push({
    id: "baseline-anchor",
    capturedDeclarationRef: "decl",
    currentRevisionId: "r1",
    continuity: {
      fromRevisionId: "r1",
      toRevisionId: "r1",
      state: "unchanged",
      evidence: null,
    },
    expectedResult: {
      status: "attached",
      targetId: { ref: "decl" },
      reason: "none",
    },
  });
  const output = normalize(loaded);
  assert.equal(output.records.coverage.length, 1);
  assert.equal(output.records.controlRegions.length, 1);
  assert.equal(output.records.durableAnchors.length, 1);
  assert.equal(
    output.records.anchorResults[0].targetId,
    output.identityMap.get("decl"),
  );
  assert.equal(output.records.symbols.length, 1);
  assert.equal(
    output.records.declarationBindings[0].syntaxId,
    output.identityMap.get("decl"),
  );
  assert.equal(output.records.typeRelationships[0].kind, "extends");
  assert.equal(
    output.records.references[0].id,
    output.identityMap.get("reference"),
  );
  assert.equal(
    output.records.callBindings[0].callId,
    output.identityMap.get("call"),
  );
  assert.equal(output.records.callBindings[0].staleTarget, false);
  for (const fact of facts) assert.ok(output.recordMap.has(fact.ref), fact.ref);
  for (const [name, mutation, pattern] of [
    [
      "symbol proof",
      (x) =>
        (x.annotations[0].facts.find(
          (f) => f.ref === "sym",
        ).record.provenanceId = "pref"),
      /NORMALIZE.EVIDENCE_KIND/,
    ],
    [
      "binding proof",
      (x) =>
        (x.annotations[0].facts.find(
          (f) => f.ref === "db",
        ).record.provenanceId = "pref"),
      /NORMALIZE.EVIDENCE_KIND/,
    ],
    [
      "relationship proof",
      (x) =>
        (x.annotations[0].facts.find((f) => f.ref === "rel").provenanceRef =
          "pc"),
      /NORMALIZE.EVIDENCE_KIND/,
    ],
    [
      "wrong anchor family",
      (x) =>
        (x.annotations[0].facts.find((f) => f.ref === "cb").anchor.kind =
          "reference"),
      /NORMALIZE.JOIN_FAMILY/,
    ],
    [
      "wrong tuple",
      (x) =>
        (x.annotations[0].facts.find((f) => f.ref === "cb").anchor.contentHash =
          "a".repeat(64)),
      /NORMALIZE.FACT_TUPLE/,
    ],
    [
      "duplicate symbol target",
      (x) =>
        x.annotations[0].facts
          .find((f) => f.ref === "sym")
          .record.declarations.push(internal()),
      /NORMALIZE.SYMBOL/,
    ],
    [
      "wrong symbol scope",
      (x) =>
        (x.annotations[0].facts.find((f) => f.ref === "sym").record.key.scope =
          "document"),
      /NORMALIZE.SYMBOL/,
    ],
  ]) {
    const changed = {
      ...loaded,
      annotations: structuredClone(loaded.annotations),
    };
    mutation(changed);
    assert.throws(() => normalize(changed), pattern, name);
  }
});
test("join matching does not cross owner, family, bytes, revision or producer support", async (t) => {
  const { s, loaded } = await loadedMeasured(t);
  const measuredAnchor = {
    document: s.document,
    revisionId: "r1",
    contentHash: hash(s.source),
    range: { start: 16, end: 18 },
    kind: "reference",
  };
  const hit = {
    ref: "reference",
    id: "occ:v2:" + "1".repeat(32),
    ownerRef: "decl",
    anchor: measuredAnchor,
  };
  const index = buildAnchorIndex([hit]);
  assert.equal(
    joinAnchor(anchor(s, "reference"), index, loaded).status,
    "exact",
  );
  for (const changed of [
    { ownerRef: "other" },
    { kind: "callee" },
    { range: span(17, 18) },
  ])
    assert.equal(
      joinAnchor({ ...anchor(s, "reference"), ...changed }, index, loaded)
        .status,
      "unmatched",
    );
  assert.throws(
    () =>
      joinAnchor(
        { ...anchor(s, "reference"), contentHash: "f".repeat(64) },
        index,
        loaded,
      ),
    /JOIN.DOCUMENT/,
  );
  assert.throws(
    () =>
      joinAnchor(
        { ...anchor(s, "reference"), revisionId: "r2" },
        index,
        loaded,
      ),
    /JOIN.DOCUMENT/,
  );
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({
      kind,
      gaps: [], available: kind !== "reference",
      diagnostic: kind === "reference" ? "unsupported by native" : null,
    }),
  );
  loaded.fixture.coverageIntents = [
    {
      producerId: "other",
      document: s.document,
      revisionId: "r1",
      requestedRoles: [],
      measurementSupport: support,
    },
  ];
  assert.equal(
    joinAnchor(
      { ...anchor(s, "reference"), range: span(17, 18) },
      index,
      loaded,
    ).status,
    "unmatched",
  );
  loaded.fixture.coverageIntents[0].producerId = "native";
  assert.equal(
    joinAnchor(
      { ...anchor(s, "reference"), range: span(17, 18) },
      index,
      loaded,
    ).status,
    "unsupported",
  );
  assert.throws(
    () => joinAnchor(anchor(s, "reference"), index, loaded),
    /JOIN.SUPPORT/,
  );
});

test("same-producer contradictory targets preserve each proof and never change measured join", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    at = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    };
  const mk = (ref, proof, target) => ({
    kind: "callBinding",
    ref,
    anchor: at,
    record: {
      resolution: "resolved",
      declaredTarget: internal(),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: proof,
    },
  });
  const facts = [
    mk("one", "p1", internal()),
    mk("two", "p2", internal()),
    mk("three", "p3", internal()),
  ];
  const { loaded } = await semanticFixture(t, facts, {
    p1: "semanticReference",
    p2: "semanticReference",
    p3: "semanticReference",
  });
  assert.equal(normalize(loaded).records.callBindings.length, 3);
  // A distinct valid target is another measured declaration, not an external resolved target.
  const second = {
    ...structuredClone(loaded.native.declarations[0]),
    ref: "other",
    name: "function",
    nameRange: span(7, 15),
    header: { ...loaded.native.declarations[0].header, name: "function" },
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(7, 15), text: "function" },
    })),
  };
  second.range = span(0, 31);
  loaded.native.declarations.push(second);
  loaded.annotations[0].facts.find(
    (x) => x.ref === "two",
  ).record.declaredTarget = internal("other");
  const third = {
    ...structuredClone(second),
    ref: "third",
    parentRef: "other",
    name: "return",
    nameRange: span(23, 29),
    range: span(22, 30),
    header: { ...second.header, name: "return" },
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(23, 29), text: "return" },
    })),
  };
  loaded.native.declarations.push(third);
  loaded.annotations[0].facts.find(
    (x) => x.ref === "three",
  ).record.declaredTarget = internal("third");
  const out = normalize(loaded);
  assert.equal(out.records.callBindings.length, 3);
  assert.ok(
    out.records.callBindings.every(
      (x) =>
        x.resolution === "ambiguous" &&
        x.declaredTarget === null &&
        x.candidates.length === 3 &&
        x.join.status === "exact",
    ),
  );
  assert.deepEqual(
    new Set(out.records.callBindings.map((x) => x.provenanceId)),
    new Set(["p1", "p2", "p3"]),
  );
  const changed = {
    ...loaded,
    annotations: structuredClone(loaded.annotations),
  };
  changed.annotations[0].facts.find((x) => x.ref === "two").record.dispatch =
    "dynamic";
  assert.throws(() => normalize(changed), /NORMALIZE.BINDING_CONFLICT/);
});
test("nested source-derived keys and actual Unicode/CRLF byte offsets", async (t) => {
  const { s, loaded } = await loadedMeasured(t);
  const parent = loaded.native.declarations[0];
  const child = {
    ...structuredClone(parent),
    ref: "child",
    parentRef: "decl",
    name: "return",
    header: { ...parent.header, name: "return" },
    range: span(22, 30),
    nameRange: span(23, 29),
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(23, 29), text: "return" },
    })),
  };
  loaded.native.declarations.push(child);
  const result = normalize(loaded);
  assert.deepEqual(
    result.records.declarations.find((x) => x.name === "return").ancestors,
    [result.records.declarations.find((x) => x.name === "go").key],
  );
  child.parentRef = null;
  assert.throws(() => normalize(loaded), /NORMALIZE.OWNER/);
  const root = await specimen();
  t.after(root.cleanup);
  const unicode = "// é\r\nexport function go() { return 1; }\r\n";
  root.source = unicode;
  root.files["src/go.js"] = unicode;
  const shifted = row(root);
  shifted.range = span(7, 39);
  shifted.nameRange = span(23, 25);
  shifted.witnesses = ["name", "header.name"].map((field) => ({
    field,
    witness: { range: span(23, 25), text: "go" },
  }));
  setNative(root, [shifted]);
  await root.flush();
  const u = await loadFixture(root.root);
  assert.equal(normalize(u).records.declarations[0].name, "go");
  shifted.nameRange = span(24, 26);
  u.native.declarations[0] = shifted;
  assert.throws(() => normalize(u), /NORMALIZE.WITNESS|WITNESS.BYTES/);
});

test("isolated r1/r2 comparison keeps historical provenance and never rebinds r1 call to r2", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    at = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    };
  const fact = {
    kind: "callBinding",
    ref: "old-call",
    anchor: at,
    record: {
      resolution: "resolved",
      declaredTarget: internal(),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "old-proof",
    },
  };
  const { s, loaded: original } = await semanticFixture(t, [fact], {
    "old-proof": "semanticReference",
  });
  const r2 = structuredClone(s.fixture.revisions[0]);
  r2.id = "r2";
  r2.documents = [
    { ...r2.documents[0], revisionId: "r2", sourceFile: "snapshots/r2.js" },
  ];
  s.fixture.revisions.push(r2);
  s.fixture.comparison = { ...s.fixture.comparison, revisionId: "r2" };
  const cloned = measured(s);
  for (const group of ["declarations", "calls", "references"])
    for (const entry of cloned[group]) {
      entry.ref += "-r2";
      entry.revisionId = "r2";
      if (entry.ownerRef) entry.ownerRef += "-r2";
    }
  const native = JSON.parse(s.files["captures/native.json"]);
  native.declarations.push(...cloned.declarations);
  native.calls.push(...cloned.calls);
  native.references.push(...cloned.references);
  s.files["captures/native.json"] = JSON.stringify(native);
  for (const [label, text, expected] of [
    ["changed", "export function go() { return 2; }\n", "stale"],
    ["same", s.source, "possiblyStale"],
  ]) {
    s.files["snapshots/r2.js"] = text;
    s.files["expected/anchors.json"] = JSON.stringify({
      formatVersion: 1,
      cases: [
        {
          id: "r1-r2-case",
          capturedDeclarationRef: "decl",
          currentRevisionId: "r2",
          continuity: {
            fromRevisionId: "r1",
            toRevisionId: "r2",
            state: text === s.source ? "unchanged" : "changed",
            evidence: null,
          },
          expectedResult: {
            status: "attached",
            targetId: { ref: "decl-r2" },
            reason: "none",
          },
        },
      ],
    });
    await s.flush();
    const result = normalize(await loadFixture(s.root));
    assert.equal(result.records.anchorResults.length, 1);
    assert.equal(
      result.records.anchorResults[0].targetId,
      result.identityMap.get("decl-r2"),
    );
    assert.equal(
      result.records.provenance.find((x) => x.id === "old-proof").freshness,
      expected,
      label,
    );
    assert.equal(result.records.callBindings.length, 1);
    assert.equal(result.recordMap.get("decl").revisionId, "r1");
    assert.equal(result.recordMap.get("decl-r2").revisionId, "r2");
    assert.equal(result.recordMap.get("decl-r2").document.path, "src/go.js");
    assert.equal(
      result.records.callBindings[0].callId,
      result.identityMap.get("call"),
    );
    // Decision 0003: identical r2 bytes share r1's occ:v2 call ID, changed
    // bytes do not; either way the single binding stays r1-scoped and is
    // never carried onto the r2 call record.
    (label === "same" ? assert.equal : assert.notEqual)(
      result.records.callBindings[0].callId,
      result.identityMap.get("call-r2"),
      label,
    );
    assert.equal(result.records.callBindings[0].join.anchor.revisionId, "r1");
    assert.equal(result.recordMap.get("call-r2").revisionId, "r2");
    assert.equal(result.recordMap.get("call").revisionId, "r1");
    // v1: evaluated at the binding's own revision (#52), so never stale.
    assert.equal(result.records.callBindings[0].staleTarget, false);
    assert.equal(
      result.identityMap.get("decl"),
      result.identityMap.get("decl-r2"),
    );
    const permuted = await loadFixture(s.root);
    for (const group of ["declarations", "calls", "references"])
      permuted.native[group].reverse();
    const alternate = normalize(permuted);
    assert.deepEqual(
      alternate.records.declarations.map((x) => [x.revisionId, x.syntaxId]),
      result.records.declarations.map((x) => [x.revisionId, x.syntaxId]),
    );
    for (const ref of ["decl", "decl-r2", "call", "call-r2"])
      assert.equal(
        JSON.stringify(alternate.recordMap.get(ref)),
        JSON.stringify(result.recordMap.get(ref)),
        ref,
      );
  }
  assert.equal(
    normalize(original).records.provenance.find((x) => x.id === "old-proof")
      .freshness,
    "fresh",
  );
});

test("all three semantic joins retain non-exact diagnostics without fabricated occurrence or binding", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    base = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      range: span(16, 18),
      ownerRef: "decl",
    };
  const facts = [
    {
      kind: "declarationBinding",
      ref: "db",
      anchor: { ...base, kind: "declarationName" },
      record: { symbols: [symbol()], provenanceId: "pdb" },
    },
    {
      kind: "reference",
      ref: "ref",
      anchor: { ...base, kind: "reference" },
      record: {
        site: "use",
        roles: ["read"],
        resolution: "unresolved",
        declaredTarget: null,
        candidates: [],
        provenanceId: "pref",
      },
    },
    {
      kind: "callBinding",
      ref: "binding-fact",
      anchor: { ...base, kind: "callee" },
      record: {
        resolution: "unresolved",
        declaredTarget: null,
        candidates: [],
        dispatch: "unknown",
        possibleDispatch: [],
        possibleDispatchComplete: false,
        provenanceId: "pc",
      },
    },
  ];
  const { loaded } = await semanticFixture(t, facts, {
    pdb: "declarationBinding",
    pref: "semanticReference",
    pc: "semanticReference",
  });
  const exact = normalize(loaded);
  assert.equal(exact.records.references.length, 1);
  assert.equal(exact.records.callBindings[0].join.status, "exact");
  assert.equal(exact.records.declarationBindings[0].join.status, "exact");
  const changed = {
    ...loaded,
    annotations: structuredClone(loaded.annotations),
  };
  for (const fact of changed.annotations[0].facts)
    if (fact.anchor) fact.anchor.range = span(18, 19);
  const nonexact = normalize(changed);
  assert.equal(nonexact.records.references.length, 0);
  assert.equal(
    nonexact.records.referenceJoinDiagnostics[0].join.status,
    "unmatched",
  );
  assert.equal(nonexact.records.declarationBindings[0].syntaxId, null);
  assert.equal(nonexact.records.callBindings[0].callId, null);
  assert.ok(nonexact.records.callBindings[0].join.diagnostic);
  const invocation = {
    ...loaded,
    annotations: structuredClone(loaded.annotations),
  };
  const call = invocation.annotations[0].facts.find(
    (x) => x.ref === "binding-fact",
  );
  call.anchor.kind = "invocation";
  call.anchor.range = span(16, 20);
  assert.equal(
    normalize(invocation).records.callBindings[0].join.status,
    "exact",
  );
});

test("all three relationship kinds preserve direction and independent semantic proof on admitted input", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" };
  const rel = {
    kind: "typeRelationship",
    ref: "relation",
    relationshipKind: "extends",
    source: internal(),
    target: internal(),
    provenanceRef: "relationship-proof",
  };
  const { loaded } = await semanticFixture(t, [rel], {
    "relationship-proof": "typeRelationship",
  });
  const other = {
    ...structuredClone(loaded.native.declarations[0]),
    ref: "base",
    name: "function",
    nameRange: span(7, 15),
    header: { ...loaded.native.declarations[0].header, name: "function" },
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(7, 15), text: "function" },
    })),
  };
  loaded.native.declarations.push(other);
  loaded.annotations[0].facts.find((x) => x.ref === "relation").target =
    internal("base");
  for (const kind of ["extends", "implements", "overrides"]) {
    const copy = {
      ...loaded,
      annotations: structuredClone(loaded.annotations),
    };
    copy.annotations[0].facts.find(
      (x) => x.ref === "relation",
    ).relationshipKind = kind;
    const out = normalize(copy),
      value = out.records.typeRelationships[0];
    assert.equal(value.kind, kind);
    assert.equal(value.source.syntaxId, out.identityMap.get("decl"));
    assert.equal(value.target.syntaxId, out.identityMap.get("base"));
    assert.notEqual(value.source.syntaxId, value.target.syntaxId);
    assert.equal(value.provenanceId, "relationship-proof");
  }
  const swapped = {
    ...loaded,
    annotations: structuredClone(loaded.annotations),
  };
  swapped.annotations[0].facts.find((x) => x.ref === "relation").source = {
    kind: "external",
    symbol: symbol(),
  };
  assert.throws(
    () => normalize(swapped),
    /NORMALIZE.RELATIONSHIP|FORMAT.SHAPE/,
  );
  const wrong = { ...loaded, annotations: structuredClone(loaded.annotations) };
  wrong.annotations[0].facts.find((x) => x.ref === "relation").provenanceRef =
    "missing-proof";
  assert.throws(() => normalize(wrong), /NORMALIZE.PROVENANCE/);
});
test("anchor case must link captured and current revision and a measured current target", async (t) => {
  const { loaded } = await loadedMeasured(t);
  loaded.anchors.cases = [
    {
      id: "case",
      capturedDeclarationRef: "decl",
      currentRevisionId: "r1",
      continuity: {
        fromRevisionId: "r1",
        toRevisionId: "r1",
        state: "unknown",
        evidence: null,
      },
      expectedResult: {
        status: "attached",
        targetId: { ref: "decl" },
        reason: "none",
      },
    },
  ];
  const valid = normalize(loaded);
  assert.equal(
    valid.records.anchorResults[0].targetId,
    valid.identityMap.get("decl"),
  );
  const wrong = { ...loaded, anchors: structuredClone(loaded.anchors) };
  wrong.anchors.cases[0].expectedResult.targetId = "sid:v1:" + "a".repeat(32);
  assert.throws(() => normalize(wrong), /NORMALIZE.ANCHOR/);
  loaded.anchors.cases[0].currentRevisionId = "r2";
  loaded.anchors.cases[0].continuity.toRevisionId = "r2";
  assert.throws(() => normalize(loaded), /NORMALIZE.ANCHOR/);
});

test("coverage tuple, state, roles, diagnostics and producer partitions", async (t) => {
  const { loaded } = await loadedMeasured(t),
    base = {
      producerId: "native",
      language: "javascript",
      sourceSetId: "main",
      documentPath: "src/go.js",
      revisionId: "r1",
      requested: true,
      selected: true,
      state: "complete",
      supportedRoles: ["read"],
      observedRoles: ["read"],
      diagnostic: null,
    };
  loaded.annotations[0].facts.push({
    kind: "coverage",
    ref: "coverage",
    record: base,
  });
  loaded.fixture.coverageIntents.push({
    producerId: "native",
    document: loaded.native.declarations[0].document,
    revisionId: "r1",
    requestedRoles: ["read"],
    measurementSupport: [
      "declarationName",
      "callee",
      "invocation",
      "reference",
    ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
  });
  assert.equal(normalize(loaded).records.coverage[0].state, "complete");
  for (const [field, value] of [
    ["producerId", "missing"],
    ["sourceSetId", "other"],
    ["documentPath", "missing.js"],
    ["requested", false],
    ["selected", false],
    ["state", "failed"],
    ["diagnostic", "unexpected"],
    ["observedRoles", ["call"]],
  ]) {
    const copy = {
      ...loaded,
      annotations: structuredClone(loaded.annotations),
    };
    copy.annotations[0].facts.at(-1).record[field] = value;
    assert.throws(() => normalize(copy), /NORMALIZE.COVERAGE/, field);
  }
  const two = {
    ...loaded,
    annotations: structuredClone(loaded.annotations),
    fixture: structuredClone(loaded.fixture),
  };
  two.fixture.coverageIntents.push({
    ...structuredClone(two.fixture.coverageIntents[0]),
    producerId: "semantic",
  });
  two.annotations[0].facts.push({
    kind: "coverage",
    ref: "semantic-coverage",
    record: {
      ...base,
      producerId: "semantic",
      state: "failed",
      diagnostic: "refresh unavailable",
      observedRoles: [],
    },
  });
  const result = normalize(two);
  assert.equal(result.records.coverage.length, 2);
  assert.deepEqual(
    new Set(result.records.coverage.map((x) => x.producerId)),
    new Set(["native", "semantic"]),
  );
});

test("two semantic producers retain separate proof and binding partitions on the same measured call", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    a = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    };
  const first = {
    kind: "callBinding",
    ref: "from-first",
    anchor: a,
    record: {
      resolution: "resolved",
      declaredTarget: internal(),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "proof-first",
    },
  };
  const { s, loaded } = await semanticFixture(t, [first], {
    "proof-first": "semanticReference",
  });
  const second = structuredClone(first);
  second.ref = "from-second";
  second.record.provenanceId = "proof-second";
  const secondArtifact = JSON.stringify({
    formatVersion: 1,
    producerId: "semantic-two",
    facts: [second],
  });
  s.files["captures/second.json"] = secondArtifact;
  s.fixture.semanticArtifacts.push("captures/second.json");
  s.fixture.captures.push({
    ref: "second-fact",
    kind: "semanticArtifact",
    file: "captures/second.json",
    hash: hash(secondArtifact),
  });
  s.files["captures/semantic-two.bin"] = "semantic two";
  s.fixture.captures.push({
    ref: "semantic-two",
    kind: "executable",
    file: "captures/semantic-two.bin",
    hash: hash("semantic two"),
  });
  const producer = {
    ...s.fixture.producers[1],
    id: "semantic-two",
    executableHash: hash("semantic two"),
  };
  s.fixture.producers.push(producer);
  s.fixture.comparison.producers.push(producer);
  const ann = JSON.parse(s.files["src/go.js.annotations.json"]);
  const p = structuredClone(
    ann.facts.find((x) => x.kind === "provenance").record,
  );
  p.id = "proof-second";
  p.producerId = "semantic-two";
  p.basis.producerId = "semantic-two";
  p.basis.producerHash = producer.executableHash;
  p.basis.artifactHash = hash(secondArtifact);
  ann.facts.push(
    { kind: "provenance", ref: "p-proof-second", record: p },
    second,
  );
  s.files["src/go.js.annotations.json"] = JSON.stringify(ann);
  await s.flush();
  const outcome = normalize(await loadFixture(s.root));
  assert.equal(outcome.records.callBindings.length, 2);
  assert.deepEqual(
    new Set(outcome.records.callBindings.map((x) => x.provenanceId)),
    new Set(["proof-first", "proof-second"]),
  );
  assert.equal(
    outcome.records.callBindings[0].callId,
    outcome.records.callBindings[1].callId,
  );
  assert.equal(normalize(loaded).records.callBindings.length, 1);
});

test("unchanged caller with changed target bytes has a possiblyStale proof; v1 staleTarget stays false", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    a = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    };
  const binding = {
    kind: "callBinding",
    ref: "target-binding",
    anchor: a,
    record: {
      resolution: "resolved",
      declaredTarget: internal("target-r1"),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "target-proof",
    },
  };
  const { s } = await semanticFixture(t, [binding], {
    "target-proof": "semanticReference",
  });
  const targetDoc = {
      sourceSetId: "main",
      language: "javascript",
      path: "src/target.js",
    },
    targetSource = "function target() {}\n";
  s.files["src/target.js"] = targetSource;
  s.files["snapshots/target-r2.js"] = "function target() { return 2; }\n";
  s.files["snapshots/caller-r2.js"] = s.source;
  s.fixture.revisions[0].documents.push({
    key: targetDoc,
    revisionId: "r1",
    sourceFile: "src/target.js",
  });
  s.fixture.revisions.push({
    ...structuredClone(s.fixture.revisions[0]),
    id: "r2",
    documents: [
      { key: d, revisionId: "r2", sourceFile: "snapshots/caller-r2.js" },
      {
        key: targetDoc,
        revisionId: "r2",
        sourceFile: "snapshots/target-r2.js",
      },
    ],
  });
  s.fixture.comparison = { ...s.fixture.comparison, revisionId: "r2" };
  const native = JSON.parse(s.files["captures/native.json"]);
  for (const [rev, ref] of [
    ["r1", "target-r1"],
    ["r2", "target-r2"],
  ])
    native.declarations.push({
      ref,
      nativeId: null,
    role: "production",
      document: targetDoc,
      revisionId: rev,
      parentRef: null,
      kind: "function",
      name: "target",
      range: span(0, 20),
      nameRange: span(9, 15),
      header: {
        kind: "function",
        name: "target",
        modifiers: [],
        typeParameters: [],
        parameters: [],
        resultType: null,
        bases: [],
      },
      signature: null,
      witnesses: ["name", "header.name"].map((field) => ({
        field,
        witness: { range: span(9, 15), text: "target" },
      })),
    });
  s.files["captures/native.json"] = JSON.stringify(native);
  s.files["src/target.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: targetDoc,
    revisionId: "r1",
    scenarios: [],
    facts: [],
  });
  s.files["snapshots/caller-r2.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: d,
    revisionId: "r2",
    scenarios: [],
    facts: [],
  });
  s.files["snapshots/target-r2.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: targetDoc,
    revisionId: "r2",
    scenarios: [],
    facts: [],
  });
  s.fixture.annotationFiles.push(
    "src/target.js.annotations.json",
    "snapshots/caller-r2.js.annotations.json",
    "snapshots/target-r2.js.annotations.json",
  );
  await s.flush();
  const preliminary = await loadFixture(s.root);
  const ann = JSON.parse(s.files["src/go.js.annotations.json"]);
  ann.facts.find(
    (x) => x.kind === "provenance",
  ).record.basis.sourceManifestHash = preliminary.sourceManifestHash(
    preliminary.revisions.get(JSON.stringify(["main", "r1"])),
  );
  s.files["src/go.js.annotations.json"] = JSON.stringify(ann);
  await s.flush();
  const output = normalize(await loadFixture(s.root));
  assert.equal(
    output.records.provenance.find((x) => x.id === "target-proof").freshness,
    "possiblyStale",
  );
  // v1: the r1 binding's target shares its revision, so it is never stale (#52).
  assert.equal(output.records.callBindings[0].staleTarget, false);
  assert.equal(output.records.callBindings[0].declaredTarget.revisionId, "r1");
});

test("producer conventions bind native ranges, witnesses and semantic anchors", async (t) => {
  const { loaded } = await loadedMeasured(t);
  for (const [field, mutate] of [
    ["range", (n) => (n.declarations[0].range.encoding = "utf16")],
    ["nameRange", (n) => (n.declarations[0].nameRange.encoding = "utf16")],
    [
      "witness",
      (n) => (n.declarations[0].witnesses[0].witness.range.encoding = "utf16"),
    ],
    ["callee", (n) => (n.calls[0].calleeRange.encoding = "unicodeScalar")],
  ]) {
    const copy = { ...loaded, native: structuredClone(loaded.native) };
    mutate(copy.native);
    assert.throws(() => normalize(copy), /NORMALIZE.ENCODING/, field);
  }
  const fact = {
    kind: "callBinding",
    ref: "encoding-binding",
    anchor: {
      document: loaded.native.declarations[0].document,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    },
    record: {
      resolution: "unresolved",
      declaredTarget: null,
      candidates: [],
      dispatch: "unknown",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "p",
    },
  };
  const { loaded: semantic } = await semanticFixture(t, [fact], {
    p: "semanticReference",
  });
  semantic.annotations[0].facts.find(
    (x) => x.ref === "encoding-binding",
  ).anchor.range.encoding = "utf16";
  assert.throws(() => normalize(semantic), /NORMALIZE.ENCODING/);
});
test("coverage intent requires partial diagnostic for unsupported or missing requested role", async (t) => {
  const { loaded } = await loadedMeasured(t),
    doc = loaded.native.declarations[0].document;
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({ kind, gaps: [], available: true, diagnostic: null }),
  );
  loaded.fixture.coverageIntents = [
    {
      producerId: "native",
      document: doc,
      revisionId: "r1",
      requestedRoles: ["read", "write"],
      measurementSupport: support,
    },
  ];
  const base = {
    producerId: "native",
    language: "javascript",
    sourceSetId: "main",
    documentPath: "src/go.js",
    revisionId: "r1",
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["read"],
    observedRoles: ["read"],
    diagnostic: null,
  };
  loaded.annotations[0].facts.push({
    kind: "coverage",
    ref: "coverage-intent",
    record: base,
  });
  assert.throws(() => normalize(loaded), /NORMALIZE.COVERAGE/);
  base.state = "partial";
  base.diagnostic = "write unsupported";
  assert.equal(normalize(loaded).records.coverage[0].state, "partial");
  base.supportedRoles = ["read", "write"];
  assert.equal(normalize(loaded).records.coverage[0].state, "partial");
  base.observedRoles = ["read", "write"];
  base.state = "complete";
  base.diagnostic = null;
  assert.equal(normalize(loaded).records.coverage[0].state, "complete");
  support[0].available = false;
  support[0].diagnostic = "unavailable";
  assert.equal(normalize(loaded).records.coverage[0].state, "complete");
  support[3].available = false;
  support[3].diagnostic = "reference unavailable";
  assert.throws(() => normalize(loaded), /NORMALIZE.COVERAGE/);
});
test("cross-namespace references and external relationship key cannot shadow valid maps", async (t) => {
  const fact = {
    kind: "typeRelationship",
    ref: "relation-fact",
    relationshipKind: "extends",
    source: internal(),
    target: { kind: "external", symbol: symbol() },
    provenanceRef: "pr",
  };
  const { loaded } = await semanticFixture(t, [fact], {
    pr: "typeRelationship",
  });
  assert.equal(
    normalize(loaded).records.typeRelationships[0].target.kind,
    "external",
  );
  for (const ref of ["decl", "reference", "call"]) {
    const copy = {
      ...loaded,
      annotations: structuredClone(loaded.annotations),
    };
    copy.annotations[0].facts.find((x) => x.kind === "typeRelationship").ref =
      ref;
    assert.throws(() => normalize(copy), /NORMALIZE.RECORD_REF/);
  }
  const bad = { ...loaded, annotations: structuredClone(loaded.annotations) };
  bad.annotations[0].facts.find(
    (x) => x.kind === "typeRelationship",
  ).target.symbol = { ...symbol(), scope: "document" };
  assert.throws(() => normalize(bad), /NORMALIZE.SYMBOL/);
  const anchorCase = {
    id: "decl",
    capturedDeclarationRef: "decl",
    currentRevisionId: "r1",
    continuity: {
      fromRevisionId: "r1",
      toRevisionId: "r1",
      state: "unchanged",
      evidence: null,
    },
    expectedResult: {
      status: "attached",
      targetId: { ref: "decl" },
      reason: "none",
    },
  };
  assert.throws(
    () =>
      normalize({
        ...loaded,
        anchors: { ...loaded.anchors, cases: [anchorCase] },
      }),
    /NORMALIZE.RECORD_REF/,
  );
});

test("admitted other-source-set measured target needs an external boundary", async (t) => {
  const fact = {
    kind: "typeRelationship",
    ref: "cross-set",
    relationshipKind: "extends",
    source: internal(),
    target: internal("foreign"),
    provenanceRef: "pr",
  };
  const { s } = await semanticFixture(t, [fact], { pr: "typeRelationship" });
  const foreign = {
    ...s.document,
    sourceSetId: "dependency",
    path: "src/foreign.js",
  };
  s.fixture.sourceSets.push({
    id: "dependency",
    rootId: "dependency-root",
    languages: ["javascript"],
    dependencies: [],
  });
  s.fixture.revisions.push({
    ...structuredClone(s.fixture.revisions[0]),
    sourceSetId: "dependency",
    documents: [
      { key: foreign, revisionId: "r1", sourceFile: "src/foreign.js" },
    ],
  });
  s.files["src/foreign.js"] = s.source;
  s.files["src/foreign.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: foreign,
    revisionId: "r1",
    scenarios: [],
    facts: [],
  });
  s.fixture.annotationFiles.push("src/foreign.js.annotations.json");
  const native = JSON.parse(s.files["captures/native.json"]);
  native.declarations.push({
    ...structuredClone(native.declarations[0]),
    ref: "foreign",
    document: foreign,
  });
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.ok(loaded.revisions.has(JSON.stringify(["dependency", "r1"])));
  assert.throws(() => normalize(loaded), /NORMALIZE.TARGET.*sourceSetId/);
  loaded.annotations[0].facts.find((x) => x.ref === "cross-set").target = {
    kind: "external",
    symbol: symbol("dependency#go"),
  };
  assert.equal(
    normalize(loaded).records.typeRelationships[0].target.kind,
    "external",
  );
});
test("UTF-16 and Unicode scalar producer offsets convert against astral CRLF source", async (t) => {
  for (const convention of ["utf16", "unicodeScalar"]) {
    const s = await specimen();
    t.after(s.cleanup);
    const prefix = "// 😀\r\n",
      source = prefix + s.source;
    s.source = source;
    s.files["src/go.js"] = source;
    s.fixture.producers[0].positionEncoding = convention;
    const shift = Buffer.byteLength(prefix),
      convert = (offset) => {
        const text = Buffer.from(source).subarray(0, offset).toString("utf8");
        return convention === "utf16" ? text.length : [...text].length;
      };
    const position = (start, end) => ({
      encoding: convention,
      start: convert(start),
      end: convert(end),
    });
    const native = row(s);
    native.range = position(shift, shift + 31);
    native.nameRange = position(shift + 16, shift + 18);
    native.witnesses = ["name", "header.name"].map((field) => ({
      field,
      witness: { range: position(shift + 16, shift + 18), text: "go" },
    }));
    setNative(s, [native]);
    await s.flush();
    const out = normalize(await loadFixture(s.root));
    assert.deepEqual(out.records.declarations[0].nameRange, {
      start: shift + 16,
      end: shift + 18,
    });
    const wrong = await loadFixture(s.root);
    wrong.native.declarations[0].nameRange.encoding =
      convention === "utf16" ? "unicodeScalar" : "utf16";
    assert.throws(() => normalize(wrong), /NORMALIZE.ENCODING/);
  }
});

test("coverage requires role-specific measurement families and either call anchor family", async (t) => {
  const { loaded } = await loadedMeasured(t),
    doc = loaded.native.declarations[0].document;
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({ kind, gaps: [], available: true, diagnostic: null }),
  );
  const intent = {
    producerId: "native",
    document: doc,
    revisionId: "r1",
    requestedRoles: ["read"],
    measurementSupport: support,
  };
  loaded.fixture.coverageIntents = [intent];
  const record = {
    producerId: "native",
    language: "javascript",
    sourceSetId: "main",
    documentPath: doc.path,
    revisionId: "r1",
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["read"],
    observedRoles: ["read"],
    diagnostic: null,
  };
  loaded.annotations[0].facts.push({
    kind: "coverage",
    ref: "capabilities",
    record,
  });
  const cases = [
    { roles: ["definition"], unavailable: "reference" },
    { roles: ["definition", "alias"], unavailable: "reference" },
    { roles: ["definition"], unavailable: "declarationName" },
    { roles: ["definition", "alias"], unavailable: "declarationName" },
    { roles: ["call"], unavailable: "reference", unrelated: true },
    { roles: ["call"], unavailable: "invocation", unrelated: true },
    { roles: ["call"], unavailable: "callee", unrelated: true },
    { roles: ["read"], unavailable: "invocation", unrelated: true },
  ];
  for (const { roles, unavailable, unrelated } of cases) {
    for (const item of support) {
      item.available = item.kind !== unavailable;
      item.diagnostic = item.available
        ? null
        : `${item.kind} measurement unavailable`;
    }
    intent.requestedRoles = roles;
    record.supportedRoles = [...roles];
    record.observedRoles = [...roles];
    record.state = "complete";
    record.diagnostic = null;
    if (unrelated) {
      assert.equal(
        normalize(loaded).records.coverage[0].state,
        "complete",
        "unrelated support does not block the role",
      );
      continue;
    }
    assert.throws(
      () => normalize(loaded),
      /NORMALIZE.COVERAGE/,
      `${roles.join("+")} cannot be complete without ${unavailable}`,
    );
    record.state = "partial";
    record.observedRoles = [];
    record.diagnostic = `${unavailable} measurement unavailable`;
    const partial = normalize(loaded).records.coverage[0];
    assert.equal(partial.state, "partial");
    assert.equal(partial.diagnostic, `${unavailable} measurement unavailable`);
    record.observedRoles = [...roles];
    assert.throws(
      () => normalize(loaded),
      /NORMALIZE.COVERAGE/,
      `${roles.join("+")} cannot be observed without ${unavailable}`,
    );
  }
  for (const item of support) {
    item.available = !["callee", "invocation"].includes(item.kind);
    item.diagnostic = item.available ? null : `${item.kind} unavailable`;
  }
  intent.requestedRoles = ["call"];
  record.supportedRoles = ["call"];
  record.observedRoles = ["call"];
  record.state = "complete";
  record.diagnostic = null;
  assert.throws(
    () => normalize(loaded),
    /NORMALIZE.COVERAGE/,
    "call cannot be observed without either measured call anchor family",
  );
});

test("coverage intent reconciles all six states and unrelated unavailable families", async (t) => {
  const { loaded } = await loadedMeasured(t),
    doc = loaded.native.declarations[0].document;
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({
      kind,
      gaps: [], available: kind !== "invocation",
      diagnostic: kind === "invocation" ? "unavailable" : null,
    }),
  );
  const intent = {
    producerId: "native",
    document: doc,
    revisionId: "r1",
    requestedRoles: ["read"],
    measurementSupport: support,
  };
  loaded.fixture.coverageIntents = [intent];
  const record = {
    producerId: "native",
    language: "javascript",
    sourceSetId: "main",
    documentPath: doc.path,
    revisionId: "r1",
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["read"],
    observedRoles: ["read"],
    diagnostic: null,
  };
  loaded.annotations[0].facts.push({
    kind: "coverage",
    ref: "coverage-states",
    record,
  });
  assert.equal(normalize(loaded).records.coverage[0].state, "complete");
  const verify = (
    state,
    requestedRoles,
    supportedRoles,
    observedRoles,
    valid = true,
  ) => {
    intent.requestedRoles = requestedRoles;
    record.state = state;
    record.requested = state !== "notRequested";
    record.selected = ["failed", "partial", "complete"].includes(state);
    record.diagnostic = ["notRequested", "complete"].includes(state)
      ? null
      : "no evidence";
    record.supportedRoles = supportedRoles;
    record.observedRoles = observedRoles;
    if (valid) assert.equal(normalize(loaded).records.coverage[0].state, state);
    else assert.throws(() => normalize(loaded), /NORMALIZE.COVERAGE/, state);
  };
  verify("complete", ["read"], ["read"], ["read"]);
  verify("partial", ["read", "write"], ["read"], ["read"]);
  verify("failed", ["read"], ["read"], []);
  verify("omitted", ["read"], ["read"], []);
  verify("unsupported", ["read"], [], []);
  verify("notRequested", [], [], []);
  verify("notRequested", ["read"], [], [], false);
  verify("omitted", ["read"], [], [], false);
  verify("unsupported", ["read"], ["read"], [], false);
  verify("complete", ["read"], ["read"], [], false);
  verify("partial", ["read"], ["read"], ["read"], false);
  verify("complete", ["read"], ["read"], ["read"]);
  loaded.fixture.coverageIntents = [];
  assert.throws(() => normalize(loaded), /NORMALIZE.COVERAGE/);
  loaded.fixture.coverageIntents = [intent];
  support[3].available = false;
  support[3].diagnostic = "reference unavailable";
  assert.throws(() => normalize(loaded), /NORMALIZE.COVERAGE/);
});
test("source-derived native reference IDs produce a complete ambiguous join diagnostic at the unit seam", async (t) => {
  const { s, loaded } = await loadedMeasured(t);
  const source = "export function go() { return go; }\n";
  s.files["src/go.js"] = source;
  const native = JSON.parse(s.files["captures/native.json"]);
  native.declarations[0].range = span(0, source.length - 1);
  native.references.push({
    ...structuredClone(native.references[0]),
    ref: "second",
    range: span(source.indexOf("go", 19), source.indexOf("go", 19) + 2),
    witnesses: [
      {
        field: "spelling",
        witness: {
          range: span(source.indexOf("go", 19), source.indexOf("go", 19) + 2),
          text: "go",
        },
      },
    ],
  });
  s.files["captures/native.json"] = JSON.stringify(native);
  await s.flush();
  const admitted = await loadFixture(s.root),
    out = normalize(admitted),
    first = out.identityMap.get("reference"),
    second = out.identityMap.get("second");
  assert.notEqual(first, second);
  const a = {
    document: s.document,
    revisionId: "r1",
    contentHash: hash(source),
    kind: "reference",
    range: span(16, 18),
    ownerRef: "decl",
  };
  const measuredAnchor = {
    document: s.document,
    revisionId: "r1",
    contentHash: a.contentHash,
    kind: a.kind,
    range: { start: 16, end: 18 },
  };
  const index = buildAnchorIndex([
    { ref: "reference", id: first, ownerRef: "decl", anchor: measuredAnchor },
    { ref: "second", id: second, ownerRef: "decl", anchor: measuredAnchor },
  ]);
  const joined = joinAnchor(a, index, admitted);
  assert.deepEqual(joined, {
    anchor: measuredAnchor,
    status: "ambiguous",
    candidateIds: [first, second].sort(),
    diagnostic: "ambiguous",
  });
});

test("same-proof same-target duplicates collapse, conflicting non-target fields reject", async (t) => {
  const d = { sourceSetId: "main", language: "javascript", path: "src/go.js" },
    a = {
      document: d,
      revisionId: "r1",
      contentHash: hash("export function go() { return 1; }\n"),
      kind: "callee",
      range: span(16, 18),
      ownerRef: "decl",
    };
  const make = (ref, proof) => ({
    kind: "callBinding",
    ref,
    anchor: a,
    record: {
      resolution: "resolved",
      declaredTarget: internal(),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: proof,
    },
  });
  const { loaded } = await semanticFixture(t, [make("one", "p1")], {
    p1: "semanticReference",
  });
  loaded.annotations[0].facts.push(make("two", "p1"));
  const out = normalize(loaded);
  assert.equal(out.records.callBindings.length, 1);
  assert.equal(
    JSON.stringify(out.recordMap.get("one")),
    JSON.stringify(out.recordMap.get("two")),
  );
  loaded.annotations[0].facts.find((x) => x.ref === "two").record.dispatch =
    "dynamic";
  assert.throws(() => normalize(loaded), /NORMALIZE.BINDING_CONFLICT/);
});
test("immediate source owner and overload signatures determine nested keys and ordinals", async (t) => {
  const { loaded } = await loadedMeasured(t),
    n = loaded.native,
    parent = n.declarations[0];
  const child = {
    ...structuredClone(parent),
    ref: "child",
    parentRef: "decl",
    name: "return",
    header: { ...parent.header, name: "return" },
    range: span(22, 30),
    nameRange: span(23, 29),
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(23, 29), text: "return" },
    })),
  };
  const grandchild = {
    ...structuredClone(child),
    ref: "grandchild",
    parentRef: "child",
    range: span(23, 29),
  };
  n.declarations.push(child, grandchild);
  const good = normalize(loaded);
  assert.deepEqual(
    good.records.declarations.find(
      (x) => x.syntaxId === good.identityMap.get("grandchild"),
    ).ancestors,
    [good.recordMap.get("decl").key, good.recordMap.get("child").key],
  );
  grandchild.parentRef = "decl";
  assert.throws(() => normalize(loaded), /NORMALIZE.OWNER/);
  grandchild.parentRef = "child";
  const changed = { ...loaded, native: structuredClone(n) };
  changed.native.declarations[1].signature = {
    parameterTypes: [],
    typeParameterCount: 0,
    variadic: false,
  };
  assert.notEqual(
    normalize(changed).identityMap.get("child"),
    good.identityMap.get("child"),
  );
  const repeat = {
    ...structuredClone(child),
    ref: "overload",
    parentRef: "decl",
    range: span(22, 31),
  };
  n.declarations.pop();
  n.declarations.push(repeat);
  assert.throws(() => normalize(loaded), /IDENTITY.ORDINAL|NORMALIZE.OWNER/);
});

test("equal-name overload ordinals follow measured source order, not native row order", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const source = "function go() {}\nfunction go() {}\n";
  s.source = source;
  s.files["src/go.js"] = source;
  const make = (ref, start) => ({
    ref,
    nativeId: ref,
    role: "production",
    document: s.document,
    revisionId: "r1",
    parentRef: null,
    kind: "function",
    name: "go",
    range: span(start, start + 16),
    nameRange: span(start + 9, start + 11),
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
    witnesses: ["name", "header.name"].map((field) => ({
      field,
      witness: { range: span(start + 9, start + 11), text: "go" },
    })),
  });
  setNative(s, [make("later", 17), make("earlier", 0)]);
  await s.flush();
  const loaded = await loadFixture(s.root),
    first = normalize(loaded);
  assert.equal(first.recordMap.get("earlier").key.ordinal, 0);
  assert.equal(first.recordMap.get("later").key.ordinal, 1);
  loaded.native.declarations.reverse();
  const reverse = normalize(loaded);
  for (const ref of ["earlier", "later"])
    assert.equal(reverse.identityMap.get(ref), first.identityMap.get(ref));
  const changed = { ...loaded, native: structuredClone(loaded.native) };
  changed.native.declarations.find((x) => x.ref === "later").range = span(
    0,
    16,
  );
  changed.native.declarations.find((x) => x.ref === "later").nameRange = span(
    9,
    11,
  );
  changed.native.declarations.find((x) => x.ref === "later").witnesses = [
    "name",
    "header.name",
  ].map((field) => ({ field, witness: { range: span(9, 11), text: "go" } }));
  assert.throws(() => normalize(changed), /IDENTITY.ORDINAL/);
  const overloaded = { ...loaded, native: structuredClone(loaded.native) };
  const header = overloaded.native.declarations.find((x) => x.ref === "later");
  header.header.parameters = [{ name: "go", type: "go", variadic: false }];
  header.signature = {
    parameterTypes: ["go"],
    typeParameterCount: 0,
    variadic: false,
  };
  header.witnesses.push(
    ...[
      "header.parameters[0].name",
      "header.parameters[0].type",
      "signature.parameterTypes[0]",
    ].map((field) => ({
      field,
      witness: { range: header.nameRange, text: "go" },
    })),
  );
  assert.notEqual(
    normalize(overloaded).identityMap.get("later"),
    first.identityMap.get("later"),
  );
});
