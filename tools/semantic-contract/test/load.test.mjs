import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { contentHash } from "../identity.mjs";
import { loadFixture, discoverFixtures } from "../load.mjs";
import { sourceWitness } from "./helpers.mjs";
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
export { specimen };
test("IDENTITY.ADMISSION verifies source-derived snapshot and independent producers", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const loaded = await loadFixture(s.root);
  assert.equal(loaded.selected.documents[0].contentHash, hash(s.source));
  assert.equal(loaded.sources.size, 1);
  assert.notEqual(
    loaded.captureBytes.get("semantic").toString(),
    loaded.captureBytes.get("native").toString(),
  );
});
test("IDENTITY.DIGEST rejects changed capture and unlisted snapshot file", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  await writeFile(join(s.root, "captures/config.txt"), "changed");
  await assert.rejects(
    loadFixture(s.root),
    (error) =>
      error.assertion === "IDENTITY.DIGEST" &&
      error.message.includes(
        `expected ${s.fixture.captures.find((c) => c.ref === "config").hash}`,
      ) &&
      error.message.includes(`actual ${hash("changed")}`),
  );
  await s.flush();
  await writeFile(join(s.root, "src/unlisted.js"), "x");
  await assert.rejects(loadFixture(s.root), /IDENTITY.INVENTORY/);
  await rm(join(s.root, "src/unlisted.js"));
  await writeFile(join(s.root, "root-unlisted.js"), "x");
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
  });
});
test("IDENTITY.PATH rejects symlink source and traversal", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  await rm(join(s.root, "src/go.js"));
  await symlink("/etc/passwd", join(s.root, "src/go.js"));
  await assert.rejects(loadFixture(s.root), /IDENTITY.PATH/);
  await rm(join(s.root, "src/go.js"));
  s.fixture.revisions[0].documents[0].sourceFile = "../outside.js";
  await s.flush();
  await assert.rejects(loadFixture(s.root), /FORMAT.SHAPE/);
});
test("DISCOVERY.JAVASCRIPT_COEXISTENCE does not conflate example and corpus", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "discover-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const [path, profile] of [
    ["example", "example"],
    ["javascript", "corpus"],
  ]) {
    await mkdir(join(root, path), { recursive: true });
    await writeFile(
      join(root, path, "fixture.json"),
      JSON.stringify({
        formatVersion: 1,
        profile,
        language: "javascript",
        sourceSets: [],
        producers: [],
        revisions: [],
        comparison: { sourceSetId: "x", revisionId: "r", producers: [] },
        coverageIntents: [],
        nativeArtifact: "x",
        semanticArtifacts: [],
        annotationFiles: [],
        answersFile: "x",
        dispositionsFile: "x",
        anchorCasesFile: "x",
        captures: [],
      }),
    );
  }
  assert.deepEqual(
    (await discoverFixtures(root)).map((x) => x.slice(root.length + 1)),
    ["example", "javascript"],
  );
});

test("IDENTITY.UTF8 rejects malformed authored JSON without replacement decoding", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  await writeFile(
    join(s.root, "captures/fact.json"),
    Buffer.from([0x7b, 0xff, 0x7d]),
  );
  // Keep the outer captured hash honest: malformed UTF-8 remains an intake failure.
  s.fixture.captures.find((x) => x.ref === "fact").hash = contentHash(
    Buffer.from([0x7b, 0xff, 0x7d]),
  );
  s.files["fixture.json"] = JSON.stringify(s.fixture);
  await writeFile(join(s.root, "fixture.json"), s.files["fixture.json"]);
  await assert.rejects(loadFixture(s.root), /UTF-8|utf-8|encoded data/i);
});

test("IDENTITY.COVERAGE rejects dangling tuples and incomplete support while retaining valid intent", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const intent = {
    producerId: "semantic",
    document: s.document,
    revisionId: "r1",
    requestedRoles: ["read"],
    measurementSupport: [
      "declarationName",
      "callee",
      "invocation",
      "reference",
    ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
  };
  s.fixture.coverageIntents = [intent];
  await s.flush();
  assert.equal((await loadFixture(s.root)).fixture.coverageIntents.length, 1);
  const mutations = [
    [(x) => (x.producerId = "missing"), "IDENTITY.COVERAGE"],
    [(x) => (x.revisionId = "missing"), "IDENTITY.COVERAGE"],
    [
      (x) => (x.document = { ...x.document, path: "missing.js" }),
      "IDENTITY.COVERAGE",
    ],
    [(x) => (x.requestedRoles = ["read", "read"]), "IDENTITY.DUPLICATE"],
    [(x) => x.measurementSupport.pop(), "IDENTITY.COVERAGE"],
    [(x) => (x.measurementSupport[1].kind = "reference"), "IDENTITY.DUPLICATE"],
    [
      (x) => (x.measurementSupport[0].diagnostic = "failed"),
      "IDENTITY.COVERAGE",
    ],
  ];
  for (const [mutate, code] of mutations) {
    const copy = structuredClone(intent);
    mutate(copy);
    s.fixture.coverageIntents = [copy];
    await s.flush();
    await assert.rejects(loadFixture(s.root), { assertion: code });
  }
  s.fixture.coverageIntents = [intent, intent];
  await s.flush();
  await assert.rejects(loadFixture(s.root), { assertion: "IDENTITY.COVERAGE" });
});
test("IDENTITY.SOURCE_SET and producer descriptors require supported unique admissions", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  for (const [mutate, assertion] of [
    [
      () => (s.fixture.sourceSets[0].dependencies = ["absent"]),
      "IDENTITY.SOURCE_SET",
    ],
    [
      () => (s.fixture.sourceSets[0].dependencies = ["main", "main"]),
      "IDENTITY.DUPLICATE",
    ],
    [
      () => (s.fixture.sourceSets[0].languages = ["javascript", "javascript"]),
      "IDENTITY.DUPLICATE",
    ],
    [() => (s.fixture.producers[1].languages = ["rust"]), "IDENTITY.PRODUCER"],
    [
      () =>
        (s.fixture.comparison.producers[1].languages = [
          "javascript",
          "javascript",
        ]),
      "IDENTITY.DUPLICATE",
    ],
  ]) {
    const original = structuredClone(s.fixture);
    mutate();
    await s.flush();
    await assert.rejects(loadFixture(s.root), { assertion });
    Object.assign(s.fixture, original);
  }
});
test("IDENTITY.INVENTORY rejects unlisted root files, duplicate annotation and root symlink", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  s.fixture.annotationFiles.push(s.fixture.annotationFiles[0]);
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.DUPLICATE",
  });
  s.fixture.annotationFiles.pop();
  s.fixture.revisions[0].documents[0].sourceFile = "go.js";
  s.fixture.annotationFiles = ["go.js.annotations.json"];
  s.files["go.js"] = s.source;
  s.files["go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: s.document,
    revisionId: "r1",
    scenarios: [],
    facts: [],
  });
  delete s.files["src/go.js"];
  delete s.files["src/go.js.annotations.json"];
  await rm(join(s.root, "src"), { recursive: true });
  await s.flush();
  await writeFile(join(s.root, "unexpected.jsx"), "x");
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
  });
  await rm(join(s.root, "unexpected.jsx"));
  await writeFile(join(s.root, "rogue.dat"), "x");
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
  });
  await rm(join(s.root, "rogue.dat"));
  await symlink("/etc/passwd", join(s.root, "unexpected.js"));
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
  });
});
test("DISCOVERY.PROFILE rejects forged descriptors and nested fixture routing", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "discover-negative-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "example"), { recursive: true });
  const s = await specimen();
  t.after(s.cleanup);
  await writeFile(
    join(root, "example", "fixture.json"),
    JSON.stringify({ ...s.fixture, profile: "corpus" }),
  );
  await assert.rejects(discoverFixtures(root), {
    assertion: "DISCOVERY.PROFILE",
  });
  await rm(join(root, "example", "fixture.json"));
  await mkdir(join(root, "example", "javascript"));
  await assert.rejects(discoverFixtures(root), {
    assertion: "DISCOVERY.PROFILE",
  });
});

test("IDENTITY.COVERAGE excludes Java alias but admits Java definition and JS alias", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const intent = {
    producerId: "semantic",
    document: s.document,
    revisionId: "r1",
    requestedRoles: ["alias", "definition"],
    measurementSupport: [
      "declarationName",
      "callee",
      "invocation",
      "reference",
    ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
  };
  s.fixture.coverageIntents = [intent];
  await s.flush();
  await loadFixture(s.root);
  const java = { ...s.document, language: "java" };
  s.fixture.language = "java";
  s.fixture.sourceSets[0].languages = ["java"];
  for (const producer of [
    ...s.fixture.producers,
    ...s.fixture.comparison.producers,
  ])
    producer.languages = ["java"];
  s.fixture.revisions[0].documents[0].key = java;
  intent.document = java;
  s.files["src/go.js.annotations.json"] = JSON.stringify({
    formatVersion: 1,
    document: java,
    revisionId: "r1",
    scenarios: [],
    facts: [],
  });
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.COVERAGE",
    field: "requestedRoles",
  });
  intent.requestedRoles = ["definition"];
  await s.flush();
  assert.equal(
    (await loadFixture(s.root)).fixture.coverageIntents[0].requestedRoles[0],
    "definition",
  );
});

test("IDENTITY.CHRONOLOGY retains opaque, per-source-set authored order", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  const revisions = s.fixture.revisions;
  const revision = (id, set = "main", path = "src/go.js") => ({
    ...structuredClone(revisions[0]),
    id,
    sourceSetId: set,
    documents: [
      {
        key: { ...s.document, sourceSetId: set, path },
        revisionId: id,
        sourceFile: `snapshots/${set}/${id}.js`,
      },
    ],
  });
  const foreign = {
    id: "foreign",
    rootId: "foreign-root",
    languages: ["javascript"],
    dependencies: [],
  };
  s.fixture.sourceSets.push(foreign);
  s.fixture.revisions = [
    revision("z-old"),
    revision("x-first", "foreign", "src/other.js"),
    revision("a-middle"),
    revision("b-new"),
    revision("w-second", "foreign", "src/other.js"),
  ];
  for (const item of s.fixture.revisions)
    s.files[item.documents[0].sourceFile] = s.source;
  // This specimen tests chronology without optional annotations.
  s.fixture.annotationFiles = [];
  delete s.files["src/go.js.annotations.json"];
  delete s.files["src/go.js"];
  s.fixture.comparison.revisionId = "b-new";
  await rm(join(s.root, "src"), { recursive: true });
  await s.flush();
  const loaded = await loadFixture(s.root);
  assert.deepEqual(
    loaded.revisionChronology.get("main").map((x) => x.id),
    ["z-old", "a-middle", "b-new"],
  );
  assert.deepEqual(
    loaded.revisionChronology.get("foreign").map((x) => x.id),
    ["x-first", "w-second"],
  );
  assert.equal(loaded.selectedIndex, 2);
  s.fixture.revisions = [
    s.fixture.revisions[3],
    ...s.fixture.revisions.slice(0, 3),
    s.fixture.revisions[4],
  ];
  await s.flush();
  assert.equal((await loadFixture(s.root)).selectedIndex, 0);
  s.fixture.comparison.revisionId = "absent";
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.COMPARISON",
    field: "comparison",
  });
});

test("IDENTITY.INVENTORY closes unknown nested inputs while excluding generated bundles", async (t) => {
  const s = await specimen();
  t.after(s.cleanup);
  await mkdir(join(s.root, "other", "nested"), { recursive: true });
  await writeFile(join(s.root, "other", "nested", "rogue.js"), "unlisted");
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
    field: "other/nested/rogue.js",
  });
  await rm(join(s.root, "other"), { recursive: true });
  await mkdir(join(s.root, "generated", "bundles"), { recursive: true });
  await writeFile(
    join(s.root, "generated", "bundles", "published.json"),
    "published",
  );
  assert.equal((await loadFixture(s.root)).sources.size, 1);
  await rm(join(s.root, s.fixture.answersFile));
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
    field: s.fixture.answersFile,
  });
  s.fixture.answersFile = "generated/answers.json";
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.INVENTORY",
    field: "generated/answers.json",
  });
});

test("IDENTITY.SEMANTIC rejects a proof claiming another captured producer or source bytes", async (t) => {
  const { s, value, check } = await relationshipSpecimen("utf8");
  t.after(s.cleanup);
  await check(value);
  const annotation = JSON.parse(s.files["src/go.js.annotations.json"]);
  annotation.facts[0].record.producerId = "native";
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.SEMANTIC",
    field: "relationship",
  });
  annotation.facts[0].record.producerId = "semantic";
  annotation.facts[0].record.contentHash = hash("forged bytes");
  s.files["src/go.js.annotations.json"] = JSON.stringify(annotation);
  await s.flush();
  await assert.rejects(loadFixture(s.root), {
    assertion: "IDENTITY.SEMANTIC",
    field: "relationship",
  });
});

// The three producers declare different coordinate systems for the same captured bytes.
async function relationshipSpecimen(
  encoding,
  { duplicate = false, secondDocument = false } = {},
) {
  const s = await specimen();
  s.source =
    "// 😀\nclass Child extends Parent {}\nclass Parent {}\n" +
    (duplicate ? "class Child {}\n" : "");
  if (secondDocument) {
    s.files["src/other.js"] = s.source;
    s.fixture.revisions[0].documents.push({
      key: { ...s.document, path: "src/other.js" },
      revisionId: "r1",
      sourceFile: "src/other.js",
    });
  }
  s.files["src/go.js"] = s.source;
  s.fixture.producers[0].positionEncoding = encoding;
  s.fixture.comparison.producers[0].positionEncoding = encoding;
  const child = sourceWitness(s.source, "Child", { encoding });
  const declarationRange = {
    encoding,
    start: sourceWitness(s.source, "class Child", { encoding }).range.start,
    end: sourceWitness(s.source, "{}", { encoding }).range.end,
  };
  const declaration = {
    ref: "child",
    nativeId: null,
    role: "production",
    document: s.document,
    revisionId: "r1",
    parentRef: null,
    kind: "type",
    name: "Child",
    range: declarationRange,
    nameRange: child.range,
    header: {
      kind: "type",
      name: "Child",
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases: ["Parent"],
    },
    signature: null,
    witnesses: [
      { field: "name", witness: child },
      { field: "header.name", witness: child },
      {
        field: "header.bases[0]",
        witness: sourceWitness(s.source, "Parent", { encoding }),
      },
    ],
  };
  const fact = {
    kind: "typeRelationship",
    ref: "relationship",
    relationshipKind: "extends",
    source: { kind: "internal", declarationRef: "child", revisionId: "r1" },
    target: {
      kind: "external",
      symbol: {
        scheme: "scip",
        symbol: "Parent",
        scope: "global",
        document: null,
      },
    },
    provenanceRef: "relationship-proof",
  };
  await s.flush();
  const admitted = await loadFixture(s.root);
  const revision = s.fixture.revisions[0],
    producer = s.fixture.producers[1];
  const basis = {
    producerId: "semantic",
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    artifactHash: null,
    language: "javascript",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: admitted.sourceManifestHash(admitted.selected),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const proof = {
    kind: "provenance",
    ref: "proof-fact",
    record: {
      id: "relationship-proof",
      producerId: "semantic",
      document: s.document,
      revisionId: "r1",
      contentHash: hash(s.source),
      evidenceKind: "typeRelationship",
      basis,
      freshness: "fresh",
    },
  };
  async function check(value) {
    s.files["captures/native.json"] = JSON.stringify({
      formatVersion: 1,
      producerId: "native",
      extractionInputs: [],
      capabilityReports: [],
      declarations: [value.declaration],
      calls: [],
      controls: [],
      references: [],
    });
    const artifact = JSON.stringify({
      formatVersion: 1,
      producerId: "semantic",
      facts: [value.fact],
    });
    s.files["captures/fact.json"] = artifact;
    s.fixture.captures.find((x) => x.ref === "fact").hash = hash(artifact);
    proof.record.basis.artifactHash = hash(artifact);
    s.files["src/go.js.annotations.json"] = JSON.stringify({
      formatVersion: 1,
      document: s.document,
      revisionId: "r1",
      scenarios: [],
      facts: [proof, value.fact],
    });
    await s.flush();
    return loadFixture(s.root);
  }
  return { s, value: { declaration, fact }, check };
}

for (const encoding of ["utf8", "utf16", "unicodeScalar"]) {
  test(`IDENTITY.SEMANTIC admits source-derived ${encoding} relationship`, async (t) => {
    const { s, value, check } = await relationshipSpecimen(encoding);
    t.after(s.cleanup);
    assert.deepEqual(
      value.declaration.nameRange,
      {
        utf8: { encoding, start: 14, end: 19 },
        utf16: { encoding, start: 12, end: 17 },
        unicodeScalar: { encoding, start: 11, end: 16 },
      }[encoding],
    );
    const loaded = await check(value);
    assert.equal(
      loaded.semanticProofs.get("relationship-proof").factKind,
      "typeRelationship",
    );
    assert.equal(loaded.native.declarations[0].name, "Child");
  });
}

const relationshipMutations = [
  [
    "utf8-split-scalar",
    "utf8",
    (v) => {
      v.declaration.nameRange.start = 5;
    },
    "invalidRange",
  ],
  [
    "utf16-split-surrogate",
    "utf16",
    (v) => {
      v.declaration.nameRange.start = 4;
    },
    "invalidRange",
  ],
  [
    "scalar-out-of-bounds",
    "unicodeScalar",
    (v) => {
      v.declaration.nameRange.end = 100;
    },
    "invalidRange",
  ],
  [
    "declaration-boundary",
    "utf16",
    (v) => {
      v.declaration.range.start = 4;
    },
    "invalidRange",
  ],
  [
    "reversed-name-range",
    "utf8",
    (v) => {
      v.declaration.nameRange.end = 13;
    },
    "invalidRange",
  ],
  [
    "witness-split-scalar",
    "utf8",
    (v) => {
      v.declaration.witnesses[0].witness.range.start = 5;
    },
    "invalidRange",
  ],
  [
    "witness-out-of-bounds",
    "utf16",
    (v) => {
      v.declaration.witnesses[0].witness.range.end = 100;
    },
    "invalidRange",
  ],
  [
    "witness-outside-declaration",
    "utf8",
    (v) => {
      v.declaration.witnesses[0].witness.range = {
        encoding: "utf8",
        start: 60,
        end: 65,
      };
    },
    "invalidRecord",
  ],
  [
    "changed-witness-text",
    "utf8",
    (v) => {
      v.declaration.witnesses[0].witness.text = "Other";
    },
    "invalidRecord",
  ],
  [
    "witness-encoding-disagreement",
    "unicodeScalar",
    (v) => {
      v.declaration.witnesses[0].witness.range.encoding = "utf8";
    },
    "invalidRecord",
  ],
  [
    "name-range-disagreement",
    "utf16",
    (v) => {
      v.declaration.nameRange.encoding = "utf8";
    },
    "invalidRecord",
  ],
  [
    "different-same-spelled-declaration",
    "utf8",
    (v) => {
      v.declaration.witnesses[0].witness.range = {
        encoding: "utf8",
        start: 60,
        end: 65,
      };
    },
    "invalidRecord",
  ],
  [
    "source-ref-mismatch",
    "utf8",
    (v) => {
      v.fact.source.declarationRef = "other";
    },
    "invalidRecord",
  ],
  [
    "source-revision-mismatch",
    "utf8",
    (v) => {
      v.fact.source.revisionId = "r2";
    },
    "invalidRecord",
  ],
  [
    "source-document-mismatch",
    "utf8",
    (v) => {
      v.declaration.document.path = "src/other.js";
    },
    "invalidRecord",
  ],
];
for (const [id, encoding, mutate, expectedCode] of relationshipMutations) {
  test(`IDENTITY.SEMANTIC relationship ${id}`, async (t) => {
    const { s, value, check } = await relationshipSpecimen(encoding, {
      duplicate: [
        "different-same-spelled-declaration",
        "witness-outside-declaration",
      ].includes(id),
      secondDocument: id === "source-document-mismatch",
    });
    t.after(s.cleanup);
    const row = registerControls([
      {
        id: `F1.${id}`,
        baseline: () => value,
        check,
        mutate: (v) => {
          mutate(v);
          return v;
        },
        expectedAssertion: "IDENTITY.SEMANTIC",
        expectedCode,
        expectedField: "fact.ref",
      },
    ])[0];
    await runControl(row);
  });
}
