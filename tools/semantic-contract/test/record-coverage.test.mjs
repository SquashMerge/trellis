import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { loadFixture } from "../load.mjs";
import {
  checkCoverage,
  compareFreshnessComponents,
  assertFreshnessComponents,
} from "../record-check/coverage.mjs";
import { contentHash, sourceManifestHash } from "../identity.mjs";
import { canonicalBytes } from "../json.mjs";
import { registerControls, runControl } from "./mutations.mjs";

const hash = (text) => contentHash(Buffer.from(text));
const sort = (rows) =>
  rows.sort((a, b) => Buffer.compare(canonicalBytes(a), canonicalBytes(b)));
const document = (path, language = "javascript") => ({
  sourceSetId: "main",
  language,
  path,
});
const symbol = {
  scheme: "scip",
  symbol: "pkg target",
  scope: "global",
  document: null,
};
const plans = [
  {
    state: "complete",
    requestedRoles: ["read"],
    supportedRoles: ["read"],
    observedRoles: ["read"],
    requested: true,
    selected: true,
    diagnostic: null,
  },
  {
    state: "partial",
    requestedRoles: ["read", "type"],
    supportedRoles: ["read", "type"],
    observedRoles: ["read"],
    requested: true,
    selected: true,
    diagnostic: "some roles missing",
  },
  {
    state: "omitted",
    requestedRoles: ["read"],
    supportedRoles: ["read"],
    observedRoles: [],
    requested: true,
    selected: false,
    diagnostic: "deliberate omission",
  },
  {
    state: "unsupported",
    requestedRoles: ["read"],
    supportedRoles: [],
    observedRoles: [],
    requested: true,
    selected: false,
    diagnostic: "unsupported",
  },
  {
    state: "failed",
    requestedRoles: ["read"],
    supportedRoles: ["read"],
    observedRoles: [],
    requested: true,
    selected: true,
    diagnostic: "capture failed",
  },
  {
    state: "notRequested",
    requestedRoles: [],
    supportedRoles: [],
    observedRoles: [],
    requested: false,
    selected: false,
    diagnostic: null,
  },
];
async function specimen(
  t,
  {
    comparisonRevision = "r2",
    sameCallerBytes = false,
    requestedChange = null,
    language = "javascript",
    setId = "main",
    captureValues = {},
    secondSet = false,
    comparisonSet = setId,
    omitSelectedDocument = false,
    completeHistorical = false,
    zeroFactHistorical = false,
  } = {},
) {
  const root = await mkdtemp(join(tmpdir(), "coverage-u1-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const files = new Map(),
    put = (name, data) =>
      files.set(name, typeof data === "string" ? data : JSON.stringify(data));
  const natives = "native-executable",
    sem1 = "semantic-one",
    sem2 = "semantic-two";
  const producers = [
    ["native", "native", natives],
    ["one", "semantic", sem1],
    ["two", "semantic", sem2],
  ].map(([id, kind, bytes]) => ({
    id,
    version: "1",
    executableHash: hash(bytes),
    kind,
    languages: [language],
    positionEncoding: "utf8",
  }));
  const captures = [];
  const add = (ref, kind, path, bytes) => {
    put(path, bytes);
    captures.push({ ref, kind, file: path, hash: hash(bytes) });
  };
  for (const [i, bytes] of [natives, sem1, sem2].entries())
    add(`executable-${i}`, "executable", `captures/executable-${i}`, bytes);
  for (const [kind, text] of [
    ["toolchain", "tool"],
    ["config", "config"],
    ["dependency", "deps"],
  ])
    add(kind, kind, `captures/${kind}`, captureValues[kind] ?? text);
  const docs = [
      { sourceSetId: setId, language, path: "a.js" },
      { sourceSetId: setId, language, path: "b.js" },
    ],
    source =
      language === "java"
        ? {
            r1: ["class A {}\n", "class B {}\n"],
            r2: ["class A { int a; }\n", "class B {}\n"],
          }
        : {
            r1: ["function a() {}\n", "function b() {}\n"],
            r2: [
              sameCallerBytes
                ? "function a() {}\n"
                : "function a() { return 1; }\n",
              sameCallerBytes
                ? "function b() { return 2; }\n"
                : "function b() {}\n",
            ],
          };
  const revisions = ["r1", "r2"].map((revisionId) => ({
    id: revisionId,
    sourceSetId: setId,
    documents: docs
      .filter(
        (key) =>
          !omitSelectedDocument || revisionId !== "r2" || key.path !== "a.js",
      )
      .map((key) => {
        const i = docs.indexOf(key);
        const sourceFile = `sources/${revisionId}/${key.path}`;
        put(sourceFile, source[revisionId][i]);
        return { key, revisionId, sourceFile };
      }),
    toolchainHash: hash(captureValues.toolchain ?? "tool"),
    configHash: hash(captureValues.config ?? "config"),
    dependencyHash: hash(captureValues.dependency ?? "deps"),
  }));
  if (secondSet) {
    const key = { sourceSetId: "other", language, path: "a.js" },
      sourceFile = "sources/other/r1/a.js";
    put(sourceFile, source.r1[0]);
    revisions.push({
      id: "r1",
      sourceSetId: "other",
      documents: [{ key, revisionId: "r1", sourceFile }],
      toolchainHash: revisions[0].toolchainHash,
      configHash: revisions[0].configHash,
      dependencyHash: revisions[0].dependencyHash,
    });
  }
  const coverageIntents = [];
  const annotationFacts = new Map();
  let count = 0;
  for (const revision of revisions)
    for (const doc of revision.documents) {
      const annotationFile = `${doc.sourceFile}.annotations.json`;
      annotationFacts.set(annotationFile, []);
      for (const producer of producers) {
        const plan =
          zeroFactHistorical && producer.id === "one" && doc.key.path === "b.js"
            ? revision.id === "r1"
              ? plans[0]
              : plans[2]
            : completeHistorical &&
                producer.id === "one" &&
                revision.id === "r1" &&
                doc.key.path === "a.js"
              ? {
                  ...plans[0],
                  requestedRoles: ["read", "type"],
                  supportedRoles: ["read", "type"],
                  observedRoles: ["read", "type"],
                }
              : producer.id === "one" &&
                  revision.id === "r2" &&
                  doc.key.path === "a.js"
                ? plans[4]
                : producer.id === "two" &&
                    revision.id === "r1" &&
                    doc.key.path === "a.js"
                  ? plans[1]
                  : plans[count % plans.length];
        count++;
        const record = {
          producerId: producer.id,
          language: doc.key.language,
          sourceSetId: doc.key.sourceSetId,
          documentPath: doc.key.path,
          revisionId: revision.id,
          requested: plan.requested,
          selected: plan.selected,
          state: plan.state,
          supportedRoles: plan.supportedRoles,
          observedRoles: plan.observedRoles,
          diagnostic: plan.diagnostic,
        };
        const ref = `coverage-${producer.id}-${revision.id}-${doc.key.path}`;
        annotationFacts
          .get(annotationFile)
          .push({ kind: "coverage", ref, record });
        coverageIntents.push({
          producerId: producer.id,
          document: doc.key,
          revisionId: revision.id,
          requestedRoles: plan.requestedRoles,
          measurementSupport: [
            "declarationName",
            "callee",
            "invocation",
            "reference",
          ].map((kind) => ({ kind, gaps: [], available: true, diagnostic: null })),
        });
      }
    }
  const rawFacts = [];
  for (const producer of producers.slice(1)) {
    const ref = `symbol-${producer.id}`,
      proofId = `proof-${producer.id}`,
      fact = {
        kind: "symbol",
        ref,
        record: {
          key: symbol,
          displayName: "target",
          declarations: [],
          provenanceId: proofId,
        },
      };
    rawFacts.push([producer, ref, proofId, fact]);
    const raw = JSON.stringify({
      formatVersion: 1,
      producerId: producer.id,
      facts: [fact],
    });
    add(
      `artifact-${producer.id}`,
      "semanticArtifact",
      `captures/${producer.id}.json`,
      raw,
    );
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
  const historical = annotationFacts.get("sources/r1/a.js.annotations.json");
  const expectedProofs = [];
  for (const [producer, ref, id, fact] of rawFacts) {
    const revision = revisions[0],
      doc = revision.documents[0].key;
    const basis = {
      producerId: producer.id,
      producerVersion: producer.version,
      producerHash: producer.executableHash,
      artifactHash: captures.find((x) => x.ref === `artifact-${producer.id}`)
        .hash,
      language,
      sourceSetId: setId,
      revisionId: "r1",
      sourceManifestHash: sourceManifestHash(
        revision.documents.map((x, i) => ({
          document: x.key,
          contentHash: hash(source.r1[i]),
        })),
      ),
      toolchainHash: revision.toolchainHash,
      configHash: revision.configHash,
      dependencyHash: revision.dependencyHash,
      lookupDependencies: ["target"],
    };
    const proof = {
      id,
      producerId: producer.id,
      document: doc,
      revisionId: "r1",
      contentHash: hash(source.r1[0]),
      evidenceKind: "declarationBinding",
      basis,
      freshness:
        comparisonSet !== setId
          ? "possiblyStale"
          : omitSelectedDocument
            ? "stale"
            : comparisonRevision === "r1"
              ? requestedChange && producer.id === "one"
                ? "possiblyStale"
                : "fresh"
              : sameCallerBytes
                ? "possiblyStale"
                : "stale",
    };
    historical.push(
      { kind: "provenance", ref: `fact-${id}`, record: proof },
      fact,
    );
    expectedProofs.push(proof);
  }
  for (const [name, facts] of annotationFacts) {
    const parts = name.split("/"),
      revisionId = parts.at(-2),
      path = parts.at(-1);
    put(name, {
      formatVersion: 1,
      document: {
        sourceSetId: parts.length === 4 ? "other" : setId,
        language,
        path: path.replace(".annotations.json", ""),
      },
      revisionId,
      scenarios: [],
      facts,
    });
  }
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  const requested = structuredClone(producers);
  if (requestedChange) {
    if (requestedChange === "absent") requested.splice(1, 1);
    if (requestedChange === "id") requested[1].id = "alternate-one";
    if (requestedChange === "version") requested[1].version = "2";
    if (requestedChange === "kind") requested[1].kind = "native";
    if (requestedChange === "hash")
      requested[1].executableHash = hash("different executable");
    if (requestedChange === "encoding") requested[1].positionEncoding = "utf16";
    if (requestedChange === "languages")
      requested[1].languages = ["javascript", "python"];
  }
  const fixture = {
    formatVersion: 1,
    profile: "example",
    language,
    sourceSets: [
      { id: setId, rootId: "root", languages: [language], dependencies: [] },
      ...(secondSet
        ? [
            {
              id: "other",
              rootId: "other-root",
              languages: [language],
              dependencies: [],
            },
          ]
        : []),
    ],
    producers,
    revisions,
    comparison: {
      sourceSetId: comparisonSet,
      revisionId: comparisonRevision,
      producers: requested,
    },
    coverageIntents,
    nativeArtifact: "captures/native.json",
    semanticArtifacts: rawFacts.map((x) => `captures/${x[0].id}.json`),
    annotationFiles: [...annotationFacts.keys()],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  put("fixture.json", fixture);
  for (const [name, value] of files) {
    await mkdir(dirname(join(root, name)), { recursive: true });
    await writeFile(join(root, name), value);
  }
  const loaded = await loadFixture(root);
  const records = {
    formatVersion: 1,
    comparison: structuredClone(fixture.comparison),
    producers: sort(structuredClone(producers)),
    sourceSets: sort(structuredClone(fixture.sourceSets)),
    revisions: sort(
      revisions.map((revision) => ({
        ...revision,
        documents: revision.documents.map((item, i) => ({
          key: item.key,
          revisionId: revision.id,
          contentHash: hash(files.get(item.sourceFile)),
          byteLength: Buffer.byteLength(files.get(item.sourceFile)),
        })),
      })),
    ),
    coverage: sort(
      [...annotationFacts.values()].flatMap((facts) =>
        facts.filter((x) => x.kind === "coverage").map((x) => x.record),
      ),
    ),
    provenance: sort(expectedProofs),
    declarations: [],
    symbols: [],
    declarationBindings: [],
    typeRelationships: [],
    calls: [],
    controlRegions: [],
    references: [],
    referenceJoinDiagnostics: [],
    callBindings: [],
    durableAnchors: [],
    groupContinuities: [],
    anchorResults: [],
  };
  return { loaded, records, fixture, source, docs, proofs: expectedProofs };
}
let admitted;
function check({ records }) {
  return checkCoverage(admitted, records);
}
function change(mutator) {
  return (specimenStub) => {
    mutator(specimenStub);
    return specimenStub;
  };
}
const cases = [
  [
    "missing-coverage",
    (x) => x.records.coverage.pop(),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "extra-coverage",
    (x) =>
      x.records.coverage.push({
        ...x.records.coverage[0],
        documentPath: "extra.js",
      }),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "duplicate-coverage",
    (x) => x.records.coverage.push({ ...x.records.coverage[0] }),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "swapped-coverage-producer",
    (x) => (x.records.coverage[0].producerId = "foreign"),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "swapped-coverage-document",
    (x) => (x.records.coverage[0].documentPath = "changed.js"),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "swapped-coverage-revision",
    (x) => (x.records.coverage[0].revisionId = "r3"),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "swapped-coverage-set",
    (x) => (x.records.coverage[0].sourceSetId = "other"),
    "COVERAGE.TUPLE",
    "coverage",
  ],
  [
    "requested-bit",
    (x) =>
      (x.records.coverage.find((y) => y.state === "complete").requested =
        false),
    "COVERAGE.STATE",
    "coverage",
  ],
  [
    "selected-bit",
    (x) =>
      (x.records.coverage.find((y) => y.state === "complete").selected = false),
    "COVERAGE.STATE",
    "coverage",
  ],
  [
    "diagnostic-bit",
    (x) =>
      (x.records.coverage.find((y) => y.state === "complete").diagnostic =
        "unexpected"),
    "COVERAGE.STATE",
    "coverage",
  ],
  [
    "state-bit",
    (x) =>
      (x.records.coverage.find((y) => y.state === "partial").state =
        "complete"),
    "COVERAGE.STATE",
    "coverage",
  ],
  [
    "role-order",
    (x) => {
      const y = x.records.coverage.find((y) => y.state === "partial");
      y.supportedRoles.reverse();
    },
    "COVERAGE.ROLES",
    "coverage.supportedRoles",
  ],
  [
    "role-duplicate",
    (x) =>
      x.records.coverage
        .find((y) => y.state === "complete")
        .observedRoles.push("read"),
    "COVERAGE.ROLES",
    "coverage.observedRoles",
  ],
  [
    "role-subset",
    (x) =>
      (x.records.coverage.find((y) => y.state === "complete").observedRoles = [
        "type",
      ]),
    "COVERAGE.ROLES",
    "coverage.observedRoles",
  ],
  [
    "identity-format",
    (x) => (x.records.formatVersion = 2),
    "FORMAT.SHAPE",
    "NormalizedRecordsV1.formatVersion",
  ],
  [
    "identity-comparison",
    (x) => (x.records.comparison.revisionId = "r1"),
    "RECORDS.IDENTITY",
    "comparison",
  ],
  [
    "identity-producer",
    (x) => (x.records.producers[0].version = "changed"),
    "RECORDS.IDENTITY",
    "producers",
  ],
  [
    "identity-source-set",
    (x) => (x.records.sourceSets[0].rootId = "changed"),
    "RECORDS.IDENTITY",
    "sourceSets",
  ],
  [
    "identity-revision",
    (x) => (x.records.revisions[0].dependencyHash = hash("changed")),
    "RECORDS.IDENTITY",
    "revisions",
  ],
  [
    "identity-document-hash",
    (x) => (x.records.revisions[0].documents[0].contentHash = hash("changed")),
    "RECORDS.IDENTITY",
    "revisions",
  ],
  [
    "identity-document-length",
    (x) => x.records.revisions[0].documents[0].byteLength++,
    "RECORDS.IDENTITY",
    "revisions",
  ],
  [
    "roles-extra-supported",
    (x) =>
      (x.records.coverage.find((y) => y.state === "complete").supportedRoles = [
        "read",
        "type",
      ]),
    "COVERAGE.ROLES",
    "coverage.supportedRoles",
  ],
  [
    "roles-unsupported-request",
    (x) =>
      (x.records.coverage.find((y) => y.state === "partial").supportedRoles = [
        "read",
      ]),
    "COVERAGE.ROLES",
    "coverage.supportedRoles",
  ],
  [
    "basis-producer-id",
    (x) => (x.records.provenance[0].basis.producerId = "other"),
    "FRESHNESS.BASIS",
    "basis.producerId",
  ],
  [
    "basis-language",
    (x) => (x.records.provenance[0].basis.language = "python"),
    "FRESHNESS.BASIS",
    "basis.language",
  ],
  [
    "basis-source-set",
    (x) => (x.records.provenance[0].basis.sourceSetId = "other"),
    "FRESHNESS.BASIS",
    "basis.sourceSetId",
  ],
  [
    "basis-revision",
    (x) => (x.records.provenance[0].basis.revisionId = "r2"),
    "FRESHNESS.BASIS",
    "basis.revisionId",
  ],
  [
    "basis-version",
    (x) => (x.records.provenance[0].basis.producerVersion = "2"),
    "FRESHNESS.BASIS",
    "basis.producerVersion",
  ],
  [
    "basis-producer-hash",
    (x) => (x.records.provenance[0].basis.producerHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.producerHash",
  ],
  [
    "basis-artifact-hash",
    (x) => (x.records.provenance[0].basis.artifactHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.artifactHash",
  ],
  [
    "basis-manifest",
    (x) => (x.records.provenance[0].basis.sourceManifestHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.sourceManifestHash",
  ],
  [
    "basis-toolchain",
    (x) => (x.records.provenance[0].basis.toolchainHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.toolchainHash",
  ],
  [
    "basis-config",
    (x) => (x.records.provenance[0].basis.configHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.configHash",
  ],
  [
    "basis-dependency",
    (x) => (x.records.provenance[0].basis.dependencyHash = hash("changed")),
    "FRESHNESS.BASIS",
    "basis.dependencyHash",
  ],
  [
    "basis-lookup",
    (x) => (x.records.provenance[0].basis.lookupDependencies = ["z", "a"]),
    "FRESHNESS.BASIS",
    "basis.lookupDependencies",
  ],
  [
    "fresh-promotion",
    (x) => (x.records.provenance[0].freshness = "fresh"),
    "FRESHNESS.STATE",
    "freshness",
  ],
];
const controls = registerControls(
  cases.map(([id, mutate, assertion, field]) => ({
    id: `U1.${id}`,
    baseline: async () => {
      throw new Error("set by test closure");
    },
    check,
    mutate: change(mutate),
    expectedAssertion: assertion,
    expectedCode: "invalidRecord",
    expectedField: field,
  })),
);
test("source-derived three-producer, two-document, r1/r2 coverage and historical proofs", async (t) => {
  const baseline = await specimen(t);
  admitted = baseline.loaded;
  const C = check(baseline);
  assert.equal(C.coverageByTuple.size, 12);
  assert.equal(C.semanticProofsById.size, 2);
  assert.equal(
    baseline.records.coverage.find(
      (x) =>
        x.producerId === "one" &&
        x.revisionId === "r2" &&
        x.documentPath === "a.js",
    ).state,
    "failed",
  );
  assert.deepEqual(
    [...new Set(baseline.records.coverage.map((x) => x.state))].sort(),
    plans.map((x) => x.state).sort(),
  );
  const proof = baseline.records.provenance[0];
  assert.equal(
    C.checkUse({
      producerId: proof.producerId,
      document: proof.document,
      revisionId: "r1",
      provenanceIds: [proof.id],
    }).proofs[0].freshness,
    "stale",
  );
  for (const row of controls)
    row.baseline = () => ({ records: baseline.records });
  for (const row of controls) await t.test(row.id, () => runControl(row));
});

test("FRESHNESS.USE keeps captured historical tuple and rejects wrong producer, revision and missing proof", async (t) => {
  const baseline = await specimen(t),
    C = checkCoverage(baseline.loaded, baseline.records),
    proof = baseline.records.provenance[0];
  const use = {
    producerId: proof.producerId,
    document: proof.document,
    revisionId: proof.revisionId,
    provenanceIds: [proof.id],
  };
  assert.equal(C.checkUse(use).proofs[0].freshness, "stale");
  const rows = registerControls(
    [
      {
        id: "U1.use-wrong-producer",
        mutate: (x) => {
          x.producerId = "native";
          return x;
        },
        expectedField: "provenanceIds",
      },
      {
        id: "U1.use-wrong-revision",
        mutate: (x) => {
          x.revisionId = "r2";
          return x;
        },
        expectedField: "provenanceIds",
      },
      {
        id: "U1.use-wrong-proof",
        mutate: (x) => {
          x.provenanceIds = ["unknown"];
          return x;
        },
        expectedField: "provenanceIds",
      },
      {
        id: "U1.use-missing-tuple",
        mutate: (x) => {
          x.document.path = "absent.js";
          return x;
        },
        expectedField: "coverage",
      },
    ].map((row) => ({
      ...row,
      baseline: () => use,
      check: (x) => C.checkUse(x),
      expectedAssertion: "FRESHNESS.USE",
      expectedCode: "invalidRecord",
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
});
test("FRESHNESS.TARGET verifies the captured declaration; v1 staleTarget is false", async (t) => {
  const baseline = await specimen(t),
    C = checkCoverage(baseline.loaded, baseline.records),
    proof = baseline.records.provenance[0];
  const syntaxId = `sid:v1:${"a".repeat(32)}`;
  const target = {
    kind: "internal",
    syntaxId,
    document: baseline.docs[0],
    revisionId: "r1",
  };
  const anchor = {
    document: baseline.docs[0],
    revisionId: "r1",
    contentHash: proof.contentHash,
    range: { start: 0, end: 1 },
    kind: "callee",
  };
  const binding = {
    callId: null,
    join: {
      anchor,
      status: "unmatched",
      candidateIds: [],
      diagnostic: "unmatched",
    },
    resolution: "resolved",
    declaredTarget: target,
    candidates: [],
    dispatch: "direct",
    possibleDispatch: [],
    possibleDispatchComplete: false,
    staleTarget: false,
    provenanceId: proof.id,
  };
  const declarations = new Map([
    ['["main","r1"]', new Map([[syntaxId, target.document]])],
  ]);
  // v1: a present target at the binding's own revision is never stale.
  assert.equal(C.checkTarget(binding, proof, declarations), false);
  const rows = registerControls(
    [
      {
        id: "U1.target-true-invalid",
        mutate: (x) => {
          x.binding.staleTarget = true;
          return x;
        },
        expectedField: "staleTarget",
      },
      {
        id: "U1.target-missing-captured",
        mutate: (x) => {
          x.declarations = new Map();
          return x;
        },
        expectedField: "declaredTarget",
      },
      {
        id: "U1.target-wrong-proof",
        mutate: (x) => {
          x.binding.provenanceId = "unknown";
          return x;
        },
        expectedField: "provenanceId",
      },
    ].map((row) => ({
      ...row,
      baseline: () => ({ binding, declarations }),
      // Wrapped: a valid v1 result is false, and the harness needs a truthy baseline.
      check: (x) => ({
        staleTarget: C.checkTarget(x.binding, proof, x.declarations),
      }),
      expectedAssertion: "FRESHNESS.TARGET",
      expectedCode: "invalidRecord",
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
  const external = {
    ...binding,
    declaredTarget: null,
    staleTarget: null,
    resolution: "unresolved",
  };
  assert.equal(C.checkTarget(external, proof, declarations), null);
});

test("identical caller bytes with changed revision or manifest are possiblyStale, not fresh", async (t) => {
  const baseline = await specimen(t, { sameCallerBytes: true }),
    C = checkCoverage(baseline.loaded, baseline.records);
  assert.equal(C.semanticProofsById.size, 2);
  assert.equal(baseline.records.provenance[0].freshness, "possiblyStale");
  const first = baseline.records.provenance[0];
  assert.equal(
    C.checkUse({
      producerId: first.producerId,
      document: first.document,
      revisionId: "r1",
      provenanceIds: [first.id],
    }).proofs[0].freshness,
    "possiblyStale",
  );
  const promoted = { ...baseline, records: structuredClone(baseline.records) };
  promoted.records.provenance[0].freshness = "fresh";
  assert.throws(
    () => checkCoverage(promoted.loaded, promoted.records),
    (e) => e.assertion === "FRESHNESS.STATE" && e.field === "freshness",
  );
});
test("captured semantic proof is fresh when every requested component matches", async (t) => {
  const baseline = await specimen(t, { comparisonRevision: "r1" }),
    C = checkCoverage(baseline.loaded, baseline.records);
  assert.equal(baseline.records.provenance[0].freshness, "fresh");
  const proof = baseline.records.provenance[0];
  assert.equal(
    C.checkUse({
      producerId: proof.producerId,
      document: proof.document,
      revisionId: "r1",
      provenanceIds: [proof.id],
    }).proofs[0].freshness,
    "fresh",
  );
});

test("requested producer components do not fall back to captured identity", async (t) => {
  for (const change of [
    "absent",
    "version",
    "hash",
    "encoding",
    "languages",
    "kind",
  ])
    await t.test(change, async (t) => {
      const baseline = await specimen(t, {
        comparisonRevision: "r1",
        requestedChange: change,
      });
      const C = checkCoverage(baseline.loaded, baseline.records);
      const proof = baseline.records.provenance.find(
        (x) => x.producerId === "one",
      );
      assert.equal(proof.freshness, "possiblyStale");
      assert.equal(
        C.checkUse({
          producerId: "one",
          document: proof.document,
          revisionId: "r1",
          provenanceIds: [proof.id],
        }).proofs[0].freshness,
        "possiblyStale",
      );
      const promoted = structuredClone(baseline.records);
      promoted.provenance.find((x) => x.id === proof.id).freshness = "fresh";
      assert.throws(
        () => checkCoverage(baseline.loaded, promoted),
        (e) =>
          e.assertion === "FRESHNESS.STATE" &&
          e.code === "invalidRecord" &&
          e.field === "freshness",
      );
    });
});

test("role-family support cannot be replaced by a coverage label", async (t) => {
  const baseline = await specimen(t);
  const validateFamily = (x) =>
    checkCoverage(
      {
        ...baseline.loaded,
        fixture: { ...baseline.loaded.fixture, coverageIntents: x.intents },
      },
      x.records,
    );
  const rows = registerControls(
    [
      {
        id: "U1.reference-family-unavailable",
        mutate: (x) => {
          const intent = x.intents.find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.document.path === "a.js",
          );
          intent.measurementSupport.find(
            (y) => y.kind === "reference",
          ).available = false;
          intent.measurementSupport.find(
            (y) => y.kind === "reference",
          ).diagnostic = "unavailable";
          return x;
        },
        expectedAssertion: "COVERAGE.ROLES",
        expectedField: "coverage.observedRoles",
      },
      {
        id: "U1.requested-role-swap",
        mutate: (x) => {
          const intent = x.intents.find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.document.path === "a.js",
          );
          intent.requestedRoles = ["type"];
          return x;
        },
        expectedAssertion: "COVERAGE.ROLES",
        expectedField: "coverage.observedRoles",
      },
    ].map((row) => ({
      ...row,
      baseline: () => ({
        records: baseline.records,
        intents: baseline.fixture.coverageIntents,
      }),
      check: validateFamily,
      expectedCode: "invalidRecord",
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
});

test("Java alias is inapplicable even when a coverage claim names it", async (t) => {
  const baseline = await specimen(t, { language: "java" });
  assert.equal(
    checkCoverage(baseline.loaded, baseline.records).coverageByTuple.size,
    12,
  );
  const control = registerControls([
    {
      id: "U1.java-alias",
      baseline: () => ({ records: baseline.records }),
      check: (x) => checkCoverage(baseline.loaded, x.records),
      mutate: (x) => {
        x.records.coverage.find((y) => y.state === "complete").supportedRoles =
          ["read", "alias"];
        return x;
      },
      expectedAssertion: "COVERAGE.ROLES",
      expectedCode: "invalidRecord",
      expectedField: "coverage.supportedRoles",
    },
  ])[0];
  await runControl(control);
});

test("captured semantic provenance inventory closes in both directions", async (t) => {
  const b = await specimen(t);
  const rows = registerControls(
    [
      {
        id: "U1.normalized-proof-omitted",
        mutate: (x) => {
          x.provenance.pop();
          return x;
        },
      },
      {
        id: "U1.normalized-proof-conflict",
        mutate: (x) => {
          x.provenance[0].id = "other-proof";
          return x;
        },
      },
    ].map((x) => ({
      ...x,
      baseline: () => b.records,
      check: (records) => checkCoverage(b.loaded, records),
      expectedAssertion: "FRESHNESS.BASIS",
      expectedCode: "invalidRecord",
      expectedField: "provenance",
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
});

test("each admitted top-level identity field and inventory row is independently checked", async (t) => {
  const b = await specimen(t);
  const variants = [
    [
      "comparison-set",
      (x) => (x.comparison.sourceSetId = "other"),
      "comparison",
    ],
    [
      "comparison-producers",
      (x) => x.comparison.producers.reverse(),
      "comparison",
    ],
    ...[
      "id",
      "kind",
      "version",
      "executableHash",
      "languages",
      "positionEncoding",
    ].map((field) => [
      `producer-${field}`,
      (x) => {
        x.producers[0][field] =
          field === "languages"
            ? ["python"]
            : field === "executableHash"
              ? hash("different")
              : field === "kind"
                ? x.producers[0].kind === "native"
                  ? "semantic"
                  : "native"
                : field === "positionEncoding"
                  ? "utf16"
                  : "different";
      },
      "producers",
    ]),
    ["producer-row", (x) => x.producers.pop(), "producers"],
    ...["id", "rootId", "languages", "dependencies"].map((field) => [
      `source-set-${field}`,
      (x) => {
        x.sourceSets[0][field] =
          field === "languages"
            ? ["python"]
            : field === "dependencies"
              ? ["other"]
              : "other";
      },
      "sourceSets",
    ]),
    ["source-set-row", (x) => x.sourceSets.pop(), "sourceSets"],
    ...[
      "id",
      "sourceSetId",
      "toolchainHash",
      "configHash",
      "dependencyHash",
    ].map((field) => [
      `revision-${field}`,
      (x) => {
        x.revisions[0][field] = field.endsWith("Hash")
          ? hash("other")
          : "other";
      },
      "revisions",
    ]),
    ["revision-row", (x) => x.revisions.pop(), "revisions"],
    ["revision-documents", (x) => x.revisions[0].documents.pop(), "revisions"],
    [
      "document-key",
      (x) => (x.revisions[0].documents[0].key.path = "other.js"),
      "revisions",
    ],
    [
      "document-revision",
      (x) => (x.revisions[0].documents[0].revisionId = "other"),
      "revisions",
    ],
    [
      "document-hash",
      (x) => (x.revisions[0].documents[0].contentHash = hash("other")),
      "revisions",
    ],
    [
      "document-length",
      (x) => x.revisions[0].documents[0].byteLength++,
      "revisions",
    ],
  ];
  const rows = registerControls(
    variants.map(([id, mutate, field]) => ({
      id: `U1.inventory-${id}`,
      baseline: () => b.records,
      check: (records) => checkCoverage(b.loaded, records),
      mutate: (x) => {
        mutate(x);
        return x;
      },
      expectedAssertion: "RECORDS.IDENTITY",
      expectedCode: "invalidRecord",
      expectedField: field,
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
});

test("captured byte sources and digests cannot counterfeit semantic basis", async (t) => {
  const b = await specimen(t);
  const mutations = [
    ["executable-byte", "executable-1", "producerId"],
    ["semantic-artifact-byte", "artifact-one", "basis.artifactHash"],
    ["toolchain-byte", "toolchain", "basis.toolchainHash"],
    ["config-byte", "config", "basis.configHash"],
    ["dependency-byte", "dependency", "basis.dependencyHash"],
  ];
  const rows = registerControls(
    mutations.map(([id, ref, field]) => ({
      id: `U1.capture-${id}`,
      baseline: () => ({ captureBytes: b.loaded.captureBytes }),
      check: (x) =>
        checkCoverage({ ...b.loaded, captureBytes: x.captureBytes }, b.records),
      mutate: (x) => {
        x.captureBytes.set(ref, Buffer.from("altered bytes"));
        return x;
      },
      expectedAssertion: "FRESHNESS.BASIS",
      expectedCode: "invalidRecord",
      expectedField: field,
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
});

test("every captured coverage state resists independent requested, selected, diagnostic and state contradictions", async (t) => {
  const b = await specimen(t);
  const cases = [];
  for (const state of [
    "notRequested",
    "omitted",
    "unsupported",
    "failed",
    "partial",
    "complete",
  ])
    for (const field of ["requested", "selected", "diagnostic", "state"]) {
      cases.push({
        id: `U1.state-${state}-${field}`,
        baseline: () => b.records,
        check: (records) => checkCoverage(b.loaded, records),
        mutate: (records) => {
          const row = records.coverage.find((x) => x.state === state);
          row[field] =
            field === "state"
              ? state === "complete"
                ? "failed"
                : "complete"
              : field === "diagnostic"
                ? row.diagnostic === null
                  ? "unexpected"
                  : null
                : !row[field];
          return records;
        },
        expectedAssertion: "COVERAGE.STATE",
        expectedCode: "invalidRecord",
        expectedField: "coverage",
      });
    }
  for (const row of registerControls(cases))
    await t.test(row.id, () => runControl(row));
});

test("all role families require their own admitted measurement support", async (t) => {
  const b = await specimen(t);
  const roleFamilies = {
    definition: ["reference", "declarationName"],
    read: ["reference"],
    write: ["reference"],
    type: ["reference"],
    import: ["reference"],
    alias: ["reference", "declarationName"],
  };
  for (const [role, families] of Object.entries(roleFamilies))
    for (const family of families) {
      const loaded = {
        ...b.loaded,
        fixture: {
          ...b.loaded.fixture,
          coverageIntents: structuredClone(b.loaded.fixture.coverageIntents),
        },
        annotations: structuredClone(b.loaded.annotations),
      };
      const records = structuredClone(b.records);
      const row = records.coverage.find(
        (x) =>
          x.producerId === "native" &&
          x.revisionId === "r1" &&
          x.documentPath === "a.js",
      );
      const intent = loaded.fixture.coverageIntents.find(
        (x) =>
          x.producerId === "native" &&
          x.revisionId === "r1" &&
          x.document.path === "a.js",
      );
      const authored = loaded.annotations
        .flatMap((x) => x.facts)
        .find(
          (x) =>
            x.kind === "coverage" &&
            x.record.producerId === "native" &&
            x.record.revisionId === "r1" &&
            x.record.documentPath === "a.js",
        ).record;
      Object.assign(row, {
        requested: true,
        selected: true,
        state: "complete",
        supportedRoles: [role],
        observedRoles: [role],
        diagnostic: null,
      });
      Object.assign(authored, row);
      intent.requestedRoles = [role];
      assert.equal(checkCoverage(loaded, records).coverageByTuple.size, 12);
      const control = registerControls([
        {
          id: `U1.family-${role}-${family}`,
          baseline: () => ({ intents: loaded.fixture.coverageIntents }),
          check: (x) =>
            checkCoverage(
              {
                ...loaded,
                fixture: { ...loaded.fixture, coverageIntents: x.intents },
              },
              records,
            ),
          mutate: (x) => {
            x.intents
              .find(
                (y) =>
                  y.producerId === "native" &&
                  y.revisionId === "r1" &&
                  y.document.path === "a.js",
              )
              .measurementSupport.find((y) => y.kind === family).available =
              false;
            return x;
          },
          expectedAssertion: "COVERAGE.ROLES",
          expectedCode: "invalidRecord",
          expectedField: "coverage.observedRoles",
        },
      ])[0];
      await t.test(control.id, () => runControl(control));
    }
});

test("call capability uses measured callee OR invocation, never neither", async (t) => {
  const b = await specimen(t);
  for (const [name, callee, invocation, allowed] of [
    ["callee-only", true, false, true],
    ["invocation-only", false, true, true],
    ["neither", false, false, false],
  ]) {
    await t.test(name, () => {
      const loaded = {
        ...b.loaded,
        fixture: {
          ...b.loaded.fixture,
          coverageIntents: structuredClone(b.loaded.fixture.coverageIntents),
        },
        annotations: structuredClone(b.loaded.annotations),
      };
      const records = structuredClone(b.records);
      const row = records.coverage.find(
        (x) =>
          x.producerId === "native" &&
          x.revisionId === "r1" &&
          x.documentPath === "a.js",
      );
      const intent = loaded.fixture.coverageIntents.find(
        (x) =>
          x.producerId === "native" &&
          x.revisionId === "r1" &&
          x.document.path === "a.js",
      );
      const authored = loaded.annotations
        .flatMap((x) => x.facts)
        .find(
          (x) =>
            x.kind === "coverage" &&
            x.record.producerId === "native" &&
            x.record.revisionId === "r1" &&
            x.record.documentPath === "a.js",
        ).record;
      for (const [family, available] of [
        ["callee", callee],
        ["invocation", invocation],
      ]) {
        const support = intent.measurementSupport.find(
          (x) => x.kind === family,
        );
        support.available = available;
        support.diagnostic = available ? null : `${family} unavailable`;
      }
      intent.requestedRoles = ["call"];
      Object.assign(row, {
        requested: true,
        selected: true,
        state: "complete",
        supportedRoles: ["call"],
        observedRoles: ["call"],
        diagnostic: null,
      });
      Object.assign(authored, row);
      if (allowed)
        assert.equal(checkCoverage(loaded, records).coverageByTuple.size, 12);
      else
        assert.throws(
          () => checkCoverage(loaded, records),
          (error) => {
            assert.equal(error.assertion, "COVERAGE.ROLES");
            assert.equal(error.field, "coverage.observedRoles");
            return true;
          },
        );
    });
  }
});

test("captured zero-fact complete tuple remains distinct from later omitted coverage", async (t) => {
  const b = await specimen(t, { zeroFactHistorical: true }),
    C = checkCoverage(b.loaded, b.records);
  const doc = b.docs[1],
    historical = C.checkUse({
      producerId: "one",
      document: doc,
      revisionId: "r1",
      provenanceIds: [],
    });
  assert.equal(historical.coverage.state, "complete");
  assert.deepEqual(historical.proofs, []);
  const current = C.checkUse({
    producerId: "one",
    document: doc,
    revisionId: "r2",
    provenanceIds: [],
  });
  assert.equal(current.coverage.state, "omitted");
  assert.equal(current.coverage.selected, false);
  assert.equal(
    C.checkUse({
      producerId: "two",
      document: doc,
      revisionId: "r1",
      provenanceIds: [],
    }).coverage.producerId,
    "two",
  );
  const control = registerControls([
    {
      id: "U1.zero-fact-captured-tuple-not-requested-r2",
      baseline: () => ({
        producerId: "one",
        document: doc,
        revisionId: "r1",
        provenanceIds: [],
      }),
      mutate: (value) => ({ ...value, provenanceIds: [b.proofs[0].id] }),
      check: (value) => C.checkUse(value),
      expectedAssertion: "FRESHNESS.USE",
      expectedCode: "invalidRecord",
      expectedField: "provenanceIds",
    },
  ])[0];
  await t.test(control.id, () => runControl(control));
});
test("both semantic producers retain historical r1 proofs despite failed r2 refresh", async (t) => {
  const b = await specimen(t),
    C = checkCoverage(b.loaded, b.records);
  assert.equal(
    b.records.coverage.find(
      (x) =>
        x.producerId === "one" &&
        x.revisionId === "r2" &&
        x.documentPath === "a.js",
    ).state,
    "failed",
  );
  for (const proof of b.records.provenance) {
    assert.equal(
      C.checkUse({
        producerId: proof.producerId,
        document: proof.document,
        revisionId: "r1",
        provenanceIds: [proof.id],
      }).proofs[0].freshness,
      "stale",
    );
    const base = {
      producerId: proof.producerId,
      document: proof.document,
      revisionId: "r2",
      provenanceIds: [],
    };
    assert.equal(C.checkUse(base).proofs.length, 0);
    const control = registerControls([
      {
        id: `U1.historical-${proof.producerId}-not-r2`,
        baseline: () => base,
        check: (x) => C.checkUse(x),
        mutate: (x) => {
          x.provenanceIds = [proof.id];
          return x;
        },
        expectedAssertion: "FRESHNESS.USE",
        expectedCode: "invalidRecord",
        expectedField: "provenanceIds",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
  const [first, second] = b.records.provenance;
  const control = registerControls([
    {
      id: "U1.cross-producer-proof",
      baseline: () => ({
        producerId: first.producerId,
        document: first.document,
        revisionId: "r1",
        provenanceIds: [first.id],
      }),
      check: (x) => C.checkUse(x),
      mutate: (x) => {
        x.provenanceIds = [second.id];
        return x;
      },
      expectedAssertion: "FRESHNESS.USE",
      expectedCode: "invalidRecord",
      expectedField: "provenanceIds",
    },
  ])[0];
  await runControl(control);
});

test("selected unsupported request and missing supported observation are distinct valid partial states", async (t) => {
  const b = await specimen(t);
  for (const [name, supported] of [
    ["unsupported-request", []],
    ["missing-observation", ["read"]],
  ]) {
    const loaded = {
        ...b.loaded,
        annotations: structuredClone(b.loaded.annotations),
      },
      records = structuredClone(b.records);
    const row = records.coverage.find((x) => x.state === "unsupported");
    const authored = loaded.annotations
      .flatMap((a) => a.facts)
      .find(
        (f) =>
          f.kind === "coverage" &&
          f.record.producerId === row.producerId &&
          f.record.revisionId === row.revisionId &&
          f.record.documentPath === row.documentPath,
      ).record;
    Object.assign(row, {
      selected: true,
      state: "partial",
      supportedRoles: supported,
      observedRoles: [],
      diagnostic: name,
    });
    Object.assign(authored, row);
    assert.equal(checkCoverage(loaded, records).coverageByTuple.size, 12);
    const control = registerControls([
      {
        id: `U1.partial-${name}-not-complete`,
        baseline: () => records,
        check: (x) => checkCoverage(loaded, x),
        mutate: (x) => {
          x.coverage.find(
            (y) =>
              y.producerId === row.producerId &&
              y.revisionId === row.revisionId &&
              y.documentPath === row.documentPath,
          ).state = "complete";
          return x;
        },
        expectedAssertion: "COVERAGE.STATE",
        expectedCode: "invalidRecord",
        expectedField: "coverage",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("requested, supported and observed role sets reject ordering, repetition, applicability and subset violations", async (t) => {
  const b = await specimen(t),
    loaded = {
      ...b.loaded,
      fixture: {
        ...b.loaded.fixture,
        coverageIntents: structuredClone(b.loaded.fixture.coverageIntents),
      },
      annotations: structuredClone(b.loaded.annotations),
    },
    records = structuredClone(b.records);
  const row = records.coverage.find(
    (x) =>
      x.producerId === "native" &&
      x.revisionId === "r1" &&
      x.documentPath === "a.js",
  );
  const intent = loaded.fixture.coverageIntents.find(
    (x) =>
      x.producerId === "native" &&
      x.revisionId === "r1" &&
      x.document.path === "a.js",
  );
  const authored = loaded.annotations
    .flatMap((x) => x.facts)
    .find(
      (x) =>
        x.kind === "coverage" &&
        x.record.producerId === "native" &&
        x.record.revisionId === "r1" &&
        x.record.documentPath === "a.js",
    ).record;
  Object.assign(row, {
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["read", "type"],
    observedRoles: ["read", "type"],
    diagnostic: null,
  });
  Object.assign(authored, row);
  intent.requestedRoles = ["read", "type"];
  const checkCase = (x) =>
    checkCoverage(
      { ...loaded, fixture: { ...loaded.fixture, coverageIntents: x.intents } },
      x.records,
    );
  const variants = [
    [
      "requested-order",
      (x) =>
        x.intents
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.document.path === "a.js",
          )
          .requestedRoles.reverse(),
      "coverage.requestedRoles",
    ],
    [
      "requested-duplicate",
      (x) =>
        x.intents
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.document.path === "a.js",
          )
          .requestedRoles.push("type"),
      "coverage.requestedRoles",
    ],
    [
      "supported-order",
      (x) =>
        x.records.coverage
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          )
          .supportedRoles.reverse(),
      "coverage.supportedRoles",
    ],
    [
      "supported-duplicate",
      (x) =>
        x.records.coverage
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          )
          .supportedRoles.push("type"),
      "coverage.supportedRoles",
    ],
    [
      "observed-order",
      (x) =>
        x.records.coverage
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          )
          .observedRoles.reverse(),
      "coverage.observedRoles",
    ],
    [
      "observed-duplicate",
      (x) =>
        x.records.coverage
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          )
          .observedRoles.push("type"),
      "coverage.observedRoles",
    ],
    [
      "observed-not-requested",
      (x) =>
        x.records.coverage
          .find(
            (y) =>
              y.producerId === "native" &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          )
          .observedRoles.push("import"),
      "coverage.observedRoles",
    ],
    [
      "observed-not-supported",
      (x) =>
        (x.records.coverage.find(
          (y) =>
            y.producerId === "native" &&
            y.revisionId === "r1" &&
            y.documentPath === "a.js",
        ).supportedRoles = ["read"]),
      "coverage.observedRoles",
    ],
  ];
  for (const [name, mutate, expectedField] of variants) {
    const control = registerControls([
      {
        id: `U1.role-set-${name}`,
        baseline: () => ({ records, intents: loaded.fixture.coverageIntents }),
        check: checkCase,
        mutate: (x) => {
          mutate(x);
          return x;
        },
        expectedAssertion: "COVERAGE.ROLES",
        expectedCode: "invalidRecord",
        expectedField,
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("required identity fields are independently rejected as format omissions", async (t) => {
  const b = await specimen(t);
  const variants = [
    [
      "comparison",
      b.records.comparison,
      ["sourceSetId", "revisionId", "producers"],
      "ComparisonContext",
    ],
    [
      "producer",
      b.records.producers[0],
      [
        "id",
        "kind",
        "version",
        "executableHash",
        "languages",
        "positionEncoding",
      ],
      "Producer",
    ],
    [
      "source-set",
      b.records.sourceSets[0],
      ["id", "rootId", "languages", "dependencies"],
      "SourceSet",
    ],
    [
      "revision",
      b.records.revisions[0],
      [
        "id",
        "sourceSetId",
        "documents",
        "toolchainHash",
        "configHash",
        "dependencyHash",
      ],
      "Revision",
    ],
    [
      "document",
      b.records.revisions[0].documents[0],
      ["key", "revisionId", "contentHash", "byteLength"],
      "RevisionDocument",
    ],
  ];
  for (const [name, _sample, fields, type] of variants)
    for (const field of fields) {
      const control = registerControls([
        {
          id: `U1.shape-${name}-${field}`,
          baseline: () => b.records,
          check: (x) => checkCoverage(b.loaded, x),
          mutate: (x) => {
            const target =
              name === "comparison"
                ? x.comparison
                : name === "producer"
                  ? x.producers[0]
                  : name === "source-set"
                    ? x.sourceSets[0]
                    : name === "revision"
                      ? x.revisions[0]
                      : x.revisions[0].documents[0];
            delete target[field];
            return x;
          },
          expectedAssertion: "FORMAT.SHAPE",
          expectedCode: "invalidRecord",
          expectedField: `NormalizedRecordsV1.${name === "comparison" ? "comparison" : name === "producer" ? "producers[0]" : name === "source-set" ? "sourceSets[0]" : name === "revision" ? "revisions[0]" : "revisions[0].documents[0]"}.${field}`,
        },
      ])[0];
      await t.test(control.id, () => runControl(control));
    }
});

test("semantic facts require selected coverage; valid historical proofs stay on their r1 tuple", async (t) => {
  const b = await specimen(t);
  const checked = checkCoverage(b.loaded, b.records);
  for (const proof of b.records.provenance) {
    assert.equal(
      checked.checkUse({
        producerId: proof.producerId,
        document: proof.document,
        revisionId: "r1",
        provenanceIds: [proof.id],
      }).proofs[0].freshness,
      "stale",
    );
    const control = registerControls([
      {
        id: `U1.fact-${proof.producerId}-unselected`,
        baseline: () => ({
          records: b.records,
          annotations: b.loaded.annotations,
        }),
        mutate: (x) => {
          const row = x.records.coverage.find(
            (y) =>
              y.producerId === proof.producerId &&
              y.revisionId === "r1" &&
              y.documentPath === "a.js",
          );
          const authored = x.annotations
            .flatMap((y) => y.facts)
            .find(
              (y) =>
                y.kind === "coverage" &&
                y.record.producerId === proof.producerId &&
                y.record.revisionId === "r1" &&
                y.record.documentPath === "a.js",
            ).record;
          Object.assign(row, {
            selected: false,
            observedRoles: [],
            state: "omitted",
            diagnostic: "unselected",
          });
          Object.assign(authored, row);
          return x;
        },
        check: (x) =>
          checkCoverage({ ...b.loaded, annotations: x.annotations }, x.records),
        expectedAssertion: "COVERAGE.FACT",
        expectedCode: "invalidRecord",
        expectedField: "coverage",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("Java alias is inapplicable in requested, supported and observed role sets", async (t) => {
  const b = await specimen(t, { language: "java" });
  const intent = (x) =>
    x.intents.find(
      (y) =>
        y.producerId === "native" &&
        y.revisionId === "r1" &&
        y.document.path === "a.js",
    );
  const row = (x) =>
    x.records.coverage.find(
      (y) =>
        y.producerId === "native" &&
        y.revisionId === "r1" &&
        y.documentPath === "a.js",
    );
  const variants = [
    [
      "requested",
      (x) => {
        intent(x).requestedRoles = ["alias"];
      },
      "coverage.requestedRoles",
    ],
    [
      "supported",
      (x) => {
        row(x).supportedRoles = ["alias"];
      },
      "coverage.supportedRoles",
    ],
    [
      "observed",
      (x) => {
        row(x).observedRoles = ["alias"];
      },
      "coverage.observedRoles",
    ],
  ];
  for (const [name, mutate, expectedField] of variants) {
    const control = registerControls([
      {
        id: `U1.java-${name}-alias`,
        baseline: () => ({
          records: b.records,
          intents: b.fixture.coverageIntents,
        }),
        check: (x) =>
          checkCoverage(
            {
              ...b.loaded,
              fixture: { ...b.loaded.fixture, coverageIntents: x.intents },
            },
            x.records,
          ),
        mutate: (x) => {
          mutate(x);
          return x;
        },
        expectedAssertion: "COVERAGE.ROLES",
        expectedCode: "invalidRecord",
        expectedField,
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("semantic proof envelope, lookup keys and capture descriptors bind to admitted bytes", async (t) => {
  const b = await specimen(t);
  const rows = registerControls(
    [
      [
        "proof-id",
        (x) => {
          x.records.provenance[0].id = "different";
        },
        "provenance",
      ],
      [
        "proof-document",
        (x) => {
          x.records.provenance[0].document = {
            ...x.records.provenance[0].document,
            path: "b.js",
          };
        },
        "contentHash",
      ],
      [
        "proof-content-hash",
        (x) => {
          x.records.provenance[0].contentHash = hash("different");
        },
        "contentHash",
      ],
      [
        "proof-evidence-kind",
        (x) => {
          x.records.provenance[0].evidenceKind = "measuredSyntax";
        },
        "basis",
      ],
      [
        "lookup-duplicate",
        (x) => {
          x.records.provenance[0].basis.lookupDependencies = [
            "target",
            "target",
          ];
        },
        "basis.lookupDependencies",
      ],
      [
        "lookup-identity",
        (x) => {
          x.records.provenance[0].basis.lookupDependencies = [
            `sid:v1:${"a".repeat(32)}`,
          ];
        },
        "basis.lookupDependencies",
      ],
    ].map(([name, mutate, field]) => ({
      id: `U1.basis-${name}`,
      baseline: () => ({ records: b.records }),
      check: (x) => checkCoverage(b.loaded, x.records),
      mutate: (x) => {
        mutate(x);
        return x;
      },
      expectedAssertion: "FRESHNESS.BASIS",
      expectedCode: "invalidRecord",
      expectedField: field,
    })),
  );
  for (const row of rows) await t.test(row.id, () => runControl(row));
  const descriptors = [
    ["executable-1", "producerId"],
    ["artifact-one", "basis.artifactHash"],
    ["toolchain", "basis.toolchainHash"],
    ["config", "basis.configHash"],
    ["dependency", "basis.dependencyHash"],
  ];
  for (const [ref, field] of descriptors) {
    const control = registerControls([
      {
        id: `U1.capture-${ref}-digest`,
        baseline: () => ({ fixture: b.loaded.fixture }),
        check: (x) =>
          checkCoverage({ ...b.loaded, fixture: x.fixture }, b.records),
        mutate: (x) => {
          x.fixture.captures.find((y) => y.ref === ref).hash =
            hash("different");
          return x;
        },
        expectedAssertion: "FRESHNESS.BASIS",
        expectedCode: "invalidRecord",
        expectedField: field,
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("v1 staleTarget: cross-revision targets are rejected, same-revision targets are never stale, others are null", async (t) => {
  const b = await specimen(t, { sameCallerBytes: true }),
    doc = b.docs[0],
    syntaxId = `sid:v1:${"b".repeat(32)}`;
  const caller = {
    id: `native:r2:sid:v1:${"a".repeat(32)}`,
    producerId: "native",
    document: doc,
    revisionId: "r2",
    contentHash: hash(b.source.r2[0]),
    evidenceKind: "measuredSyntax",
    basis: null,
    freshness: "fresh",
  };
  b.records.provenance.push(caller);
  const binding = (declaredTarget, staleTarget) => ({
    callId: null,
    join: {
      anchor: {
        document: doc,
        revisionId: "r2",
        contentHash: caller.contentHash,
        range: { start: 0, end: 1 },
        kind: "callee",
      },
      status: "unmatched",
      candidateIds: [],
      diagnostic: "unmatched",
    },
    resolution: declaredTarget ? "resolved" : "unresolved",
    declaredTarget,
    candidates: [],
    dispatch: "direct",
    possibleDispatch: [],
    possibleDispatchComplete: false,
    staleTarget,
    provenanceId: caller.id,
  });
  const internal = (revisionId) => ({
    kind: "internal",
    syntaxId,
    document: doc,
    revisionId,
  });
  const declarations = new Map([
    ['["main","r1"]', new Map([[syntaxId, doc]])],
    ['["main","r2"]', new Map([[syntaxId, doc]])],
  ]);
  const C = checkCoverage(b.loaded, b.records);
  // A current caller naming an older target is invalid in v1, whatever its flag.
  for (const flag of [true, false])
    assert.throws(
      () => C.checkTarget(binding(internal("r1"), flag), caller, declarations),
      { assertion: "FRESHNESS.TARGET", field: "declaredTarget" },
    );
  // A same-revision target is never stale, even when the comparison's bytes differ.
  const selected = structuredClone(b.loaded.selected);
  for (const row of selected.documents) row.contentHash = hash("changed");
  for (const loaded of [b.loaded, { ...b.loaded, selected }]) {
    const checker = checkCoverage(loaded, b.records);
    assert.equal(
      checker.checkTarget(binding(internal("r2"), false), caller, declarations),
      false,
    );
    const control = registerControls([
      {
        id: `U1.target-same-revision-true-bit-${loaded === b.loaded ? "base" : "changed"}`,
        baseline: () => binding(internal("r2"), false),
        check: (x) => ({
          staleTarget: checker.checkTarget(x, caller, declarations),
        }),
        mutate: (x) => {
          x.staleTarget = true;
          return x;
        },
        expectedAssertion: "FRESHNESS.TARGET",
        expectedCode: "invalidRecord",
        expectedField: "staleTarget",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
  for (const [name, target] of [
    ["external", { kind: "external", symbol }],
    ["no-target", null],
  ]) {
    const control = registerControls([
      {
        id: `U1.target-${name}-null`,
        baseline: () => binding(target, null),
        check: (x) => ({
          staleTarget: C.checkTarget(x, caller, declarations),
        }),
        mutate: (x) => {
          x.staleTarget = false;
          return x;
        },
        expectedAssertion: "FRESHNESS.TARGET",
        expectedCode: "invalidRecord",
        expectedField: "staleTarget",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
});

test("both producer uses reject absent tuples, wrong proofs and cross-revision attachment", async (t) => {
  const b = await specimen(t),
    C = checkCoverage(b.loaded, b.records);
  for (const proof of b.records.provenance) {
    const use = {
      producerId: proof.producerId,
      document: proof.document,
      revisionId: "r1",
      provenanceIds: [proof.id],
    };
    const other = b.records.provenance.find((x) => x.id !== proof.id);
    const variants = [
      [
        "absent-tuple",
        (x) => {
          x.document = { ...x.document, path: "absent.js" };
        },
        "coverage",
      ],
      [
        "wrong-tuple",
        (x) => {
          x.document = { ...x.document, path: "b.js" };
        },
        "provenanceIds",
      ],
      [
        "missing-proof",
        (x) => {
          x.provenanceIds = ["unknown"];
        },
        "provenanceIds",
      ],
      [
        "wrong-producer-proof",
        (x) => {
          x.provenanceIds = [other.id];
        },
        "provenanceIds",
      ],
      [
        "cross-revision-proof",
        (x) => {
          x.revisionId = "r2";
        },
        "provenanceIds",
      ],
    ];
    for (const [name, mutate, expectedField] of variants) {
      const control = registerControls([
        {
          id: `U1.use-${proof.producerId}-${name}`,
          baseline: () => use,
          check: (x) => C.checkUse(x),
          mutate: (x) => {
            mutate(x);
            return x;
          },
          expectedAssertion: "FRESHNESS.USE",
          expectedCode: "invalidRecord",
          expectedField,
        },
      ])[0];
      await t.test(control.id, () => runControl(control));
    }
  }
});

test("native proof uses authenticated freshness seam for fresh, stale and possiblyStale", async (t) => {
  for (const [name, options, expected] of [
    ["fresh", { comparisonRevision: "r1" }, "fresh"],
    ["stale", {}, "stale"],
    ["possiblyStale", { sameCallerBytes: true }, "possiblyStale"],
  ]) {
    const b = await specimen(t, options);
    const native = {
      id: `native-r1-${name}`,
      producerId: "native",
      document: b.docs[0],
      revisionId: "r1",
      contentHash: hash(b.source.r1[0]),
      evidenceKind: "measuredSyntax",
      basis: null,
      freshness: expected,
    };
    b.records.provenance.push(native);
    assert.equal(checkCoverage(b.loaded, b.records).coverageByTuple.size, 12);
    const control = registerControls([
      {
        id: `U1.native-${name}-false-fresh`,
        baseline: () => b.records,
        check: (x) => checkCoverage(b.loaded, x),
        mutate: (x) => {
          x.provenance.find((y) => y.id === native.id).freshness =
            expected === "fresh" ? "stale" : "fresh";
          return x;
        },
        expectedAssertion: "FRESHNESS.STATE",
        expectedCode: "invalidRecord",
        expectedField: "freshness",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
    const bad = structuredClone(b.records);
    bad.provenance.find((x) => x.id === native.id).contentHash = hash(
      "wrong captured bytes",
    );
    bad.provenance.find((x) => x.id === native.id).freshness = "fresh";
    assert.throws(
      () => checkCoverage(b.loaded, bad),
      (e) =>
        e.assertion === "FRESHNESS.BASIS" &&
        e.code === "invalidRecord" &&
        e.field === "contentHash",
    );
  }
});

test("admitted changed and missing selected bytes outrank same-byte component drift", async (t) => {
  for (const [name, options, count] of [
    ["changed-bytes", { completeHistorical: true }, 12],
    [
      "missing-bytes",
      {
        sameCallerBytes: true,
        omitSelectedDocument: true,
        completeHistorical: true,
      },
      9,
    ],
  ]) {
    const b = await specimen(t, options);
    assert.equal(
      b.loaded.fixture.revisions.find((x) => x.id === "r2").documents.length,
      name === "missing-bytes" ? 1 : 2,
    );
    assert.equal(b.loaded.annotations.length, name === "missing-bytes" ? 3 : 4);
    assert.equal(b.records.coverage.length, count);
    const C = checkCoverage(b.loaded, b.records);
    assert.equal(C.coverageByTuple.size, count);
    const proof = b.records.provenance.find((x) => x.producerId === "one");
    assert.equal(
      b.records.coverage.find(
        (x) =>
          x.producerId === "one" &&
          x.revisionId === "r1" &&
          x.documentPath === "a.js",
      ).state,
      "complete",
    );
    assert.equal(proof.freshness, "stale");
    const selected = b.loaded.revisions.get('["main","r2"]');
    if (name === "missing-bytes")
      assert.equal(
        selected.documents.some((x) => x.key.path === "a.js"),
        false,
      );
    const control = registerControls([
      {
        id: `U1.freshness-${name}-complete-stale`,
        baseline: () => b.records,
        check: (x) => checkCoverage(b.loaded, x),
        mutate: (x) => {
          x.provenance.find((y) => y.id === proof.id).freshness =
            name === "missing-bytes" ? "possiblyStale" : "fresh";
          return x;
        },
        expectedAssertion: "FRESHNESS.STATE",
        expectedCode: "invalidRecord",
        expectedField: "freshness",
      },
    ])[0];
    await t.test(control.id, () => runControl(control));
  }
  const b = await specimen(t, { comparisonRevision: "r1" }),
    proof = b.records.provenance.find((x) => x.producerId === "one");
  const row = b.records.coverage.find(
    (x) =>
      x.producerId === "one" &&
      x.revisionId === "r1" &&
      x.documentPath === "a.js",
  );
  assert.equal(row.state, "partial");
  assert.equal(proof.freshness, "fresh");
  const control = registerControls([
    {
      id: "U1.freshness-partial-fresh-not-possibly-stale",
      baseline: () => b.records,
      check: (x) => checkCoverage(b.loaded, x),
      mutate: (x) => {
        x.provenance.find((y) => y.producerId === "one").freshness =
          "possiblyStale";
        return x;
      },
      expectedAssertion: "FRESHNESS.STATE",
      expectedCode: "invalidRecord",
      expectedField: "freshness",
    },
  ])[0];
  await t.test(control.id, () => runControl(control));
});

// Component values come from independently admitted snapshots. Only the policy
// input is projected one field at a time; no synthetic loaded fixture is passed
// to checkCoverage.
test("authenticated component comparison isolates each same-byte request change", async (t) => {
  const base = await specimen(t, { comparisonRevision: "r1" });
  checkCoverage(base.loaded, base.records);
  const proof = base.records.provenance.find((x) => x.producerId === "one");
  const producer = base.loaded.fixture.producers.find((x) => x.id === "one");
  const captured = {
    documentHash: proof.contentHash,
    revisionId: proof.basis.revisionId,
    sourceManifestHash: proof.basis.sourceManifestHash,
    toolchainHash: proof.basis.toolchainHash,
    configHash: proof.basis.configHash,
    dependencyHash: proof.basis.dependencyHash,
    sourceSetId: proof.basis.sourceSetId,
    producerId: producer.id,
    producerKind: producer.kind,
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    language: proof.document.language,
    positionEncoding: producer.positionEncoding,
    producerLanguages: producer.languages,
  };
  assert.equal(
    compareFreshnessComponents(captured, structuredClone(captured)),
    "fresh",
  );
  const alternate = {
    revisionId: await specimen(t, {
      comparisonRevision: "r2",
      sameCallerBytes: true,
    }),
    sourceManifestHash: await specimen(t, {
      comparisonRevision: "r2",
      sameCallerBytes: true,
    }),
    toolchainHash: await specimen(t, {
      comparisonRevision: "r1",
      captureValues: { toolchain: "alternate tool" },
    }),
    configHash: await specimen(t, {
      comparisonRevision: "r1",
      captureValues: { config: "alternate config" },
    }),
    dependencyHash: await specimen(t, {
      comparisonRevision: "r1",
      captureValues: { dependency: "alternate dependency" },
    }),
    sourceSetId: await specimen(t, {
      comparisonRevision: "r1",
      setId: "other",
    }),
    producerId: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "id",
    }),
    producerKind: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "kind",
    }),
    producerVersion: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "version",
    }),
    producerHash: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "hash",
    }),
    language: await specimen(t, { comparisonRevision: "r1", language: "java" }),
    positionEncoding: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "encoding",
    }),
    producerLanguages: await specimen(t, {
      comparisonRevision: "r1",
      requestedChange: "languages",
    }),
  };
  const otherValue = (field, fixture) => {
    const snapshot = fixture.loaded.revisions.get(
      JSON.stringify([
        fixture.loaded.comparison.sourceSetId,
        fixture.loaded.comparison.revisionId,
      ]),
    );
    const requested = fixture.loaded.comparison.producers.find(
      (x) => x.id === "one",
    );
    switch (field) {
      case "revisionId":
        return snapshot.id;
      case "sourceManifestHash":
        return sourceManifestHash(
          snapshot.documents.map((x) => ({
            document: x.key,
            contentHash: x.contentHash,
          })),
        );
      case "toolchainHash":
      case "configHash":
      case "dependencyHash":
        return snapshot[field];
      case "sourceSetId":
        return fixture.loaded.comparison.sourceSetId;
      case "producerId":
        return fixture.loaded.comparison.producers.find(
          (x) => x.id === "alternate-one",
        ).id;
      case "producerKind":
        return requested.kind;
      case "producerVersion":
        return requested.version;
      case "producerHash":
        return requested.executableHash;
      case "language":
        return fixture.loaded.fixture.language;
      case "positionEncoding":
        return requested.positionEncoding;
      case "producerLanguages":
        return requested.languages;
    }
  };
  for (const [field, fixture] of Object.entries(alternate)) {
    checkCoverage(fixture.loaded, fixture.records);
    const changed = otherValue(field, fixture);
    assert.notDeepEqual(changed, captured[field], field);
    for (const [label, value] of [
      ["changed", changed],
      ["unavailable", null],
    ])
      await t.test(`${field}-${label}`, async () => {
        const request = { ...structuredClone(captured), [field]: value };
        assert.equal(request.documentHash, captured.documentHash);
        assert.equal(
          compareFreshnessComponents(captured, request),
          "possiblyStale",
        );
        const control = registerControls([
          {
            id: `U1.component-${field}-${label}`,
            baseline: () => ({ request, freshness: "possiblyStale" }),
            check: (x) =>
              assertFreshnessComponents(captured, x.request, x.freshness),
            mutate: (x) => {
              x.freshness = "fresh";
              return x;
            },
            expectedAssertion: "FRESHNESS.STATE",
            expectedCode: "invalidRecord",
            expectedField: "freshness",
          },
        ])[0];
        await runControl(control);
      });
  }
  const selected = base.loaded.revisions.get('["main","r1"]');
  const r2 = alternate.revisionId.loaded.revisions.get('["main","r2"]');
  const changedBytes = await specimen(t, { comparisonRevision: "r2" });
  checkCoverage(changedBytes.loaded, changedBytes.records);
  const otherHash =
    changedBytes.loaded.revisions.get('["main","r2"]').documents[0].contentHash;
  assert.notEqual(otherHash, captured.documentHash);
  for (const [label, documentHash] of [
    ["changed", otherHash],
    ["missing", null],
  ]) {
    const request = { ...captured, revisionId: r2.id, documentHash };
    assert.equal(compareFreshnessComponents(captured, request), "stale", label);
  }
  assert.equal(selected.documents[0].contentHash, captured.documentHash);
});

test("integrated admitted r1/r2 snapshots with two source sets preserve independent coverage and proofs", async (t) => {
  const baseline = await specimen(t, {
    comparisonRevision: "r1",
    secondSet: true,
  });
  const C = checkCoverage(baseline.loaded, baseline.records);
  assert.equal(C.sourceSets.size, 2);
  assert.equal(C.revisions.size, 3);
  assert.equal(C.coverageByTuple.size, 15);
  const proof = baseline.records.provenance.find((x) => x.producerId === "one");
  assert.equal(proof.freshness, "fresh");
  const promoted = structuredClone(baseline.records);
  promoted.comparison.sourceSetId = "other";
  assert.throws(
    () => checkCoverage(baseline.loaded, promoted),
    (e) => e.assertion === "RECORDS.IDENTITY" && e.field === "comparison",
  );
  const selectedOther = await specimen(t, {
    comparisonRevision: "r1",
    secondSet: true,
    comparisonSet: "other",
  });
  assert.equal(
    checkCoverage(selectedOther.loaded, selectedOther.records)
      .semanticProofsById.size,
    2,
  );
  assert.equal(selectedOther.records.provenance[0].freshness, "possiblyStale");
  const falseFresh = structuredClone(selectedOther.records);
  falseFresh.provenance[0].freshness = "fresh";
  assert.throws(
    () => checkCoverage(selectedOther.loaded, falseFresh),
    (e) =>
      e.assertion === "FRESHNESS.STATE" &&
      e.code === "invalidRecord" &&
      e.field === "freshness",
  );
  const shifted = await specimen(t, {
    setId: "other",
    comparisonRevision: "r1",
  });
  assert.equal(
    checkCoverage(shifted.loaded, shifted.records).semanticProofsById.size,
    2,
  );
  const captured = {
    documentHash: proof.contentHash,
    revisionId: proof.revisionId,
    sourceManifestHash: proof.basis.sourceManifestHash,
    toolchainHash: proof.basis.toolchainHash,
    configHash: proof.basis.configHash,
    dependencyHash: proof.basis.dependencyHash,
    sourceSetId: proof.document.sourceSetId,
    producerId: proof.producerId,
    producerKind: "semantic",
    producerVersion: proof.basis.producerVersion,
    producerHash: proof.basis.producerHash,
    language: proof.document.language,
    positionEncoding: "utf8",
    producerLanguages: ["javascript"],
  };
  assert.equal(
    assertFreshnessComponents(
      captured,
      { ...captured, sourceSetId: shifted.fixture.comparison.sourceSetId },
      "possiblyStale",
    ),
    "possiblyStale",
  );
});
