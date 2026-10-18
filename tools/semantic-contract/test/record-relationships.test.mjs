import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { loadFixture } from "../load.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import { checkMeasurement } from "../record-check/measurement.mjs";
import { checkJoins } from "../record-check/joins.mjs";
import { checkRelationships } from "../record-check/relationships.mjs";
import { registerControls, runControl } from "./mutations.mjs";

const source =
  "class Child extends Parent {}\nclass Parent {}\nclass Impl implements Face {}\ninterface Face {}\nclass Base { void method() {} }\nclass Derived extends Base { @Override void method() {} }\n";
const document = {
  sourceSetId: "main",
  language: "java",
  path: "src/Main.java",
};
const sha = (value) => createHash("sha256").update(value).digest("hex");
const canon = (value) =>
  value === null || typeof value !== "object"
    ? JSON.stringify(value)
    : Array.isArray(value)
      ? `[${value.map(canon).join(",")}]`
      : `{${Object.keys(value)
          .sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)))
          .map((k) => `${canon(k)}:${canon(value[k])}`)
          .join(",")}}`;
const digest = (domain, value) =>
  createHash("sha256")
    .update(`trellis.${domain}.${domain === "occurrence" ? "v2" : "v1"}\0`)
    .update(canon(value))
    .digest("hex")
    .slice(0, 32);
const span = (start, end) => ({ start, end, encoding: "utf8" });
const range = (start, end) => ({ start, end });
const witness = (field, start, end, text) => ({
  field,
  witness: { range: span(start, end), text },
});
const globalKey = {
  scheme: "scip",
  symbol: "pkg Child",
  scope: "global",
  document: null,
};
const externalKey = {
  scheme: "scip",
  symbol: "dependency artifact/External#v1",
  scope: "global",
  document: null,
};
const targetRef = (declarationRef) => ({
  kind: "internal",
  declarationRef,
  revisionId: "r1",
});
const names = ["Child", "Parent", "Impl", "Face", "Base", "Derived"];
function authoredDeclarations(text = source) {
  const declarations = [];
  for (const name of names) {
    const beginning = text.indexOf(
        `${name === "Face" ? "interface" : "class"} ${name} `,
      ),
      nameStart = beginning + (name === "Face" ? 10 : 6);
    const end =
      text.indexOf("}", beginning) +
      1 +
      (name === "Base" || name === "Derived" ? 2 : 0);
    const childHeader =
      name === "Child"
        ? text.slice(beginning, text.indexOf("{", beginning))
        : "";
    const bases =
      name === "Child"
        ? [
            ...childHeader.matchAll(
              /\b(?:extends|implements)\s+(Parent|External|Face)/g,
            ),
          ].map((match) => match[1])
        : name === "Impl"
          ? ["Face"]
          : name === "Derived"
            ? ["Base"]
            : [];
    const header = {
      kind: "type",
      name,
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases,
    };
    const witnesses = [
      witness("name", nameStart, nameStart + name.length, name),
      witness("header.name", nameStart, nameStart + name.length, name),
    ];
    for (const [index, base] of bases.entries()) {
      const start = text.indexOf(
        base,
        index
          ? text.indexOf(bases[index - 1], nameStart) + bases[index - 1].length
          : nameStart + name.length,
      );
      witnesses.push(
        witness(`header.bases[${index}]`, start, start + base.length, base),
      );
    }
    declarations.push({
      ref: name,
      nativeId: null,
    role: "production",
      document,
      revisionId: "r1",
      parentRef: null,
      kind: "type",
      name,
      range: span(beginning, end),
      nameRange: span(nameStart, nameStart + name.length),
      header,
      signature: null,
      witnesses,
    });
  }
  for (const parent of ["Base", "Derived"]) {
    const beginning = text.indexOf("method(", text.indexOf(`class ${parent} `)),
      name = "method",
      ref = `${parent}.method`;
    const modifier =
      parent === "Derived" &&
      text
        .slice(text.indexOf("class Derived "), beginning)
        .includes("@Override")
        ? "Override"
        : null;
    const marker = modifier
      ? text.lastIndexOf("@Override", beginning)
      : beginning;
    const parameterText = text.slice(
      beginning + 7,
      text.indexOf(")", beginning),
    );
    const parameters =
      parameterText === "int value"
        ? [{ name: "value", type: "int", variadic: false }]
        : [];
    const signature = parameters.length
      ? { parameterTypes: ["int"], typeParameterCount: 0, variadic: false }
      : null;
    const header = {
      kind: "method",
      name,
      modifiers: modifier ? [modifier] : [],
      typeParameters: [],
      parameters,
      resultType: null,
      bases: [],
    };
    declarations.push({
      ref,
      nativeId: null,
    role: "production",
      document,
      revisionId: "r1",
      parentRef: parent,
      kind: "method",
      name,
      range: span(marker, text.indexOf("}", beginning) + 1),
      nameRange: span(beginning, beginning + 6),
      header,
      signature,
      witnesses: [
        witness("name", beginning, beginning + 6, name),
        witness("header.name", beginning, beginning + 6, name),
        ...(modifier
          ? [witness("header.modifiers[0]", marker + 1, marker + 9, modifier)]
          : []),
        ...(parameters.length
          ? [
              witness(
                "header.parameters[0].name",
                beginning + 11,
                beginning + 16,
                "value",
              ),
              witness(
                "header.parameters[0].type",
                beginning + 7,
                beginning + 10,
                "int",
              ),
              witness(
                "signature.parameterTypes[0]",
                beginning + 7,
                beginning + 10,
                "int",
              ),
            ]
          : []),
      ],
    });
  }
  return declarations;
}
const declarations = authoredDeclarations(),
  byRef = new Map(declarations.map((d) => [d.ref, d]));
function idOf(ref) {
  const row = byRef.get(ref.replace(/^r2:/, "")),
    parent = row.parentRef ? byRef.get(row.parentRef) : null;
  const key = (d) => ({
    kind: d.kind,
    name: d.name,
    signature: null,
    ordinal: 0,
  });
  return `sid:v1:${digest("syntax", { sourceSet: "main", path: document.path, language: "java", ancestors: parent ? [key(parent)] : [], declaration: key(row) })}`;
}
const internal = (ref) => ({
  kind: "internal",
  syntaxId: idOf(ref),
  document,
  revisionId: "r1",
});
const external = { kind: "external", symbol: externalKey };
const relationships = [
  {
    kind: "typeRelationship",
    ref: "extends",
    relationshipKind: "extends",
    source: targetRef("Child"),
    target: targetRef("Parent"),
    provenanceRef: "proof-extends",
  },
  {
    kind: "typeRelationship",
    ref: "implements",
    relationshipKind: "implements",
    source: targetRef("Impl"),
    target: targetRef("Face"),
    provenanceRef: "proof-implements",
  },
  {
    kind: "typeRelationship",
    ref: "overrides",
    relationshipKind: "overrides",
    source: targetRef("Derived.method"),
    target: targetRef("Base.method"),
    provenanceRef: "proof-overrides",
  },
];
const symbol = {
  kind: "symbol",
  ref: "symbol",
  record: {
    key: globalKey,
    displayName: "Child",
    declarations: [targetRef("Child"), external],
    provenanceId: "proof-symbol",
  },
};
const binding = {
  kind: "declarationBinding",
  ref: "binding",
  anchor: {
    document,
    revisionId: "r1",
    contentHash: sha(source),
    kind: "declarationName",
    range: span(
      byRef.get("Child").nameRange.start,
      byRef.get("Child").nameRange.end,
    ),
    ownerRef: "Child",
  },
  record: { symbols: [globalKey], provenanceId: "proof-binding" },
};
const nativeProducer = {
  id: "native",
  version: "1",
  executableHash: sha("native executable"),
  kind: "native",
  languages: ["java"],
  positionEncoding: "utf8",
};
const semanticProducer = {
  id: "semantic",
  version: "1",
  executableHash: sha("semantic executable"),
  kind: "semantic",
  languages: ["java"],
  positionEncoding: "utf8",
};
const rowOrder = (rows) =>
  rows.sort((a, b) =>
    a.syntaxId && b.syntaxId
      ? Buffer.compare(Buffer.from(a.syntaxId), Buffer.from(b.syntaxId)) ||
        Buffer.compare(Buffer.from(a.revisionId), Buffer.from(b.revisionId))
      : a.id && b.id
        ? Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)) ||
          Buffer.compare(Buffer.from(canon(a)), Buffer.from(canon(b)))
        : Buffer.compare(Buffer.from(canon(a)), Buffer.from(canon(b))),
  );
async function specimen(t, change = {}) {
  const root = await mkdtemp(join(tmpdir(), "relationships-u4-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const text = change.source ?? source,
    nativeRows = authoredDeclarations(text),
    nativeByRef = new Map(nativeRows.map((row) => [row.ref, row]));
  const idFor = (ref) => {
    const row = nativeByRef.get(ref.replace(/^r2:/, ""));
    const parent = row.parentRef ? nativeByRef.get(row.parentRef) : null;
    const name = (d) => ({
      kind: d.kind,
      name: d.name,
      signature: d.signature,
      ordinal: 0,
    });
    return `sid:v1:${digest("syntax", { sourceSet: "main", path: document.path, language: "java", ancestors: parent ? [name(parent)] : [], declaration: name(row) })}`;
  };
  const nextText = "// r2 source\n" + text;
  const nextRows = change.history
    ? authoredDeclarations(nextText).map((row) => ({
        ...row,
        ref: `r2:${row.ref}`,
        parentRef: row.parentRef ? `r2:${row.parentRef}` : null,
        revisionId: "r2",
      }))
    : [];
  const files = new Map(),
    put = (name, value) =>
      files.set(
        name,
        typeof value === "string" ? value : JSON.stringify(value),
      );
  const facts = structuredClone([symbol, binding, ...relationships]);
  facts[1].anchor.contentHash = sha(text);
  facts[1].anchor.range = span(
    nativeByRef.get("Child").nameRange.start,
    nativeByRef.get("Child").nameRange.end,
  );
  if (change.raw) change.raw(facts);
  const baseWitness = nativeByRef
    .get("Child")
    .witnesses.find((row) => row.field === "header.bases[0]")?.witness;
  const baseReference = change.externalReference && {
    ref: "child-base-reference",
    nativeId: null,
    document,
    revisionId: "r1",
    ownerRef: "Child",
    range: baseWitness.range,
    spelling: baseWitness.text,
    witnesses: [
      witness(
        "spelling",
        baseWitness.range.start,
        baseWitness.range.end,
        baseWitness.text,
      ),
    ],
  };
  if (baseReference)
    facts.push({
      kind: "reference",
      ref: "base-reference",
      anchor: {
        document,
        revisionId: "r1",
        contentHash: sha(text),
        kind: "reference",
        range: baseReference.range,
        ownerRef: "Child",
      },
      record: {
        site: "use",
        roles: ["type"],
        resolution:
          change.externalReference.kind === "external"
            ? "external"
            : "resolved",
        declaredTarget: change.externalReference,
        candidates: [],
        provenanceId: "proof-base-reference",
      },
    });
  const nextFact = {
    ...structuredClone(relationships[0]),
    ref: "r2-extends",
    source: { ...targetRef("r2:Child"), revisionId: "r2" },
    target: {
      ...targetRef(change.historicalTarget ? "Parent" : "r2:Parent"),
      revisionId: change.historicalTarget ? "r1" : "r2",
    },
    provenanceRef: "proof-r2-extends",
  };
  const nextSymbol = {
    kind: "symbol",
    ref: "r2-symbol",
    record: {
      key: { ...globalKey, symbol: "pkg RevisedChild" },
      displayName: "RevisedChild",
      declarations: [
        {
          ...targetRef(change.historicalSymbol ? "Child" : "r2:Child"),
          revisionId: change.historicalSymbol ? "r1" : "r2",
        },
      ],
      provenanceId: "proof-r2-symbol",
    },
  };
  const nextFacts = change.history ? [nextFact, nextSymbol] : [];
  put("snapshots/src/Main.java", text);
  if (change.history) put("snapshots/r2/Main.java", nextText);
  put("captures/native.json", {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    declarations: [...nativeRows, ...nextRows],
    calls: [],
    controls: [],
    references: baseReference ? [baseReference] : [],
  });
  put("captures/semantic.json", {
    formatVersion: 1,
    producerId: "semantic",
    facts: [...facts, ...nextFacts],
  });
  for (const [name, value] of [
    ["native", "native executable"],
    ["semantic", "semantic executable"],
    ["toolchain", "toolchain"],
    ["config", "config"],
    ["dependency", "dependency"],
  ])
    put(`captures/${name}.txt`, value);
  const captures = [
    ["native", "executable", "captures/native.txt"],
    ["semantic", "executable", "captures/semantic.txt"],
    ["toolchain", "toolchain", "captures/toolchain.txt"],
    ["config", "config", "captures/config.txt"],
    ["dependency", "dependency", "captures/dependency.txt"],
    ["artifact", "semanticArtifact", "captures/semantic.json"],
  ].map(([ref, kind, file]) => ({
    ref,
    kind,
    file,
    hash: sha(files.get(file)),
  }));
  const revision = {
    id: "r1",
    sourceSetId: "main",
    documents: [
      {
        key: document,
        revisionId: "r1",
        sourceFile: "snapshots/src/Main.java",
      },
    ],
    toolchainHash: captures[2].hash,
    configHash: captures[3].hash,
    dependencyHash: captures[4].hash,
  };
  const basis = {
    producerId: "semantic",
    producerVersion: "1",
    producerHash: semanticProducer.executableHash,
    artifactHash: captures[5].hash,
    language: "java",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: sha(canon([{ document, contentHash: sha(text) }])),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const proofs = facts.map((fact) => ({
    id: fact.provenanceRef ?? fact.record.provenanceId,
    producerId: "semantic",
    document,
    revisionId: "r1",
    contentHash: sha(text),
    evidenceKind:
      fact.kind === "typeRelationship"
        ? "typeRelationship"
        : fact.kind === "reference"
          ? "semanticReference"
          : "declarationBinding",
    basis,
    freshness: change.history ? "stale" : "fresh",
  }));
  const nextRevision = {
    ...revision,
    id: "r2",
    documents: [
      { key: document, revisionId: "r2", sourceFile: "snapshots/r2/Main.java" },
    ],
  };
  const nextBasis = {
    ...basis,
    revisionId: "r2",
    sourceManifestHash: sha(canon([{ document, contentHash: sha(nextText) }])),
  };
  const nextProofs = nextFacts.map((fact) => ({
    id: fact.provenanceRef ?? fact.record.provenanceId,
    producerId: "semantic",
    document,
    revisionId: "r2",
    contentHash: sha(nextText),
    evidenceKind:
      fact.kind === "typeRelationship"
        ? "typeRelationship"
        : "declarationBinding",
    basis: nextBasis,
    freshness: "fresh",
  }));
  change.proofs?.(proofs, nextProofs);
  const coverage = (id, revisionId = "r1") => ({
    producerId: id,
    language: "java",
    sourceSetId: "main",
    documentPath: document.path,
    revisionId,
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles:
      baseReference && revisionId === "r1" && id === "semantic"
        ? ["definition", "type"]
        : ["definition"],
    observedRoles:
      baseReference && revisionId === "r1" && id === "semantic"
        ? ["definition", "type"]
        : ["definition"],
    diagnostic: null,
  });
  put("snapshots/src/Main.java.annotations.json", {
    formatVersion: 1,
    document,
    revisionId: "r1",
    scenarios: [],
    facts: [
      ...["native", "semantic"].map((id) => ({
        kind: "coverage",
        ref: `coverage-${id}`,
        record: coverage(id),
      })),
      ...proofs.map((record, i) => ({
        kind: "provenance",
        ref: `proof-fact-${i}`,
        record,
      })),
      ...facts,
    ],
  });
  if (change.history)
    put("snapshots/r2/Main.java.annotations.json", {
      formatVersion: 1,
      document,
      revisionId: "r2",
      scenarios: [],
      facts: [
        ...["native", "semantic"].map((id) => ({
          kind: "coverage",
          ref: `coverage-${id}-r2`,
          record: coverage(id, "r2"),
        })),
        ...nextProofs.map((record, i) => ({
          kind: "provenance",
          ref: `proof-r2-fact-${i}`,
          record,
        })),
        ...nextFacts,
      ],
    });
  const producers = [nativeProducer, semanticProducer];
  const fixture = {
    formatVersion: 1,
    profile: "example",
    language: "java",
    sourceSets: [
      { id: "main", rootId: "root", languages: ["java"], dependencies: [] },
    ],
    producers,
    revisions: change.history ? [revision, nextRevision] : [revision],
    comparison: {
      sourceSetId: "main",
      revisionId: change.history ? "r2" : "r1",
      producers,
    },
    coverageIntents: (change.history ? ["r1", "r2"] : ["r1"]).flatMap(
      (revisionId) =>
        producers.map((producer) => ({
          producerId: producer.id,
          document,
          revisionId,
          requestedRoles:
            baseReference && revisionId === "r1" && producer.id === "semantic"
              ? ["definition", "type"]
              : ["definition"],
          measurementSupport: [
            "declarationName",
            "callee",
            "invocation",
            "reference",
          ].map((kind) => ({ kind, available: true, diagnostic: null })),
        })),
    ),
    nativeArtifact: "captures/native.json",
    semanticArtifacts: ["captures/semantic.json"],
    annotationFiles: change.history
      ? [
          "snapshots/src/Main.java.annotations.json",
          "snapshots/r2/Main.java.annotations.json",
        ]
      : ["snapshots/src/Main.java.annotations.json"],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  for (const [name, contents] of files) {
    await mkdir(dirname(join(root, name)), { recursive: true });
    await writeFile(join(root, name), contents);
  }
  const loaded = await loadFixture(root);
  const normalizedDeclarations = [...nativeRows, ...nextRows].map((d) => ({
    syntaxId: idFor(d.ref),
    role: "production",
    document,
    revisionId: d.revisionId,
    kind: d.kind,
    name: d.name,
    lookupKey: d.name,
    ancestors: d.parentRef
      ? [
          {
            kind: "type",
            name: d.parentRef.replace(/^r2:/, ""),
            signature: null,
            ordinal: 0,
          },
        ]
      : [],
    key: { kind: d.kind, name: d.name, signature: d.signature, ordinal: 0 },
    range: range(d.range.start, d.range.end),
    nameRange: range(d.nameRange.start, d.nameRange.end),
    header: d.header,
    provenanceId: `native:${d.revisionId}:${idFor(d.ref)}`,
  }));
  const records = {
    formatVersion: 1,
    comparison: fixture.comparison,
    producers: structuredClone(producers).sort((a, b) =>
      Buffer.compare(Buffer.from(canon(a)), Buffer.from(canon(b))),
    ),
    sourceSets: fixture.sourceSets,
    revisions: [
      {
        ...revision,
        documents: [
          {
            key: document,
            revisionId: "r1",
            contentHash: sha(text),
            byteLength: Buffer.byteLength(text),
          },
        ],
      },
      ...(change.history
        ? [
            {
              ...nextRevision,
              documents: [
                {
                  key: document,
                  revisionId: "r2",
                  contentHash: sha(nextText),
                  byteLength: Buffer.byteLength(nextText),
                },
              ],
            },
          ]
        : []),
    ],
    coverage: rowOrder(
      (change.history ? ["r1", "r2"] : ["r1"]).flatMap((rev) =>
        ["native", "semantic"].map((id) => coverage(id, rev)),
      ),
    ),
    provenance: [
      ...proofs,
      ...nextProofs,
      ...[...nativeRows, ...nextRows].map((d) => ({
        id: `native:${d.revisionId}:${idFor(d.ref)}`,
        producerId: "native",
        document,
        revisionId: d.revisionId,
        contentHash: sha(d.revisionId === "r2" ? nextText : text),
        evidenceKind: "measuredSyntax",
        basis: null,
        freshness: change.history && d.revisionId === "r1" ? "stale" : "fresh",
      })),
    ],
    declarations: rowOrder(normalizedDeclarations),
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
  if (baseReference) {
    // Decision 0003: document bytes, empty-inventory extraction context and the
// native producer descriptor, not the revision, identify the occurrence.
    const id = `occ:v2:${digest("occurrence", {
      contentHash: sha(text),
      extractionContext: sha(
        "trellis.extraction-context.v1\0" +
          canon({ language: "java", components: [] }),
      ),
      nativeProducerId: "native",
      nativeProducerVersion: "1",
      ownerSyntaxId: idFor("Child"),
      kind: "reference",
      ordinal: 0,
    })}`;
    const target =
      change.externalReference.kind === "external"
        ? change.externalReference
        : internal(change.externalReference.declarationRef);
    records.references.push({
      id,
      ownerSyntaxId: idFor("Child"),
      ordinal: 0,
      document,
      revisionId: "r1",
      range: range(baseReference.range.start, baseReference.range.end),
      spelling: baseReference.spelling,
      lookupKey: baseReference.spelling,
      site: "use",
      roles: ["type"],
      resolution:
        change.externalReference.kind === "external" ? "external" : "resolved",
      declaredTarget: target,
      candidates: [],
      provenanceId: "proof-base-reference",
    });
  }
  for (const fact of [...facts, ...nextFacts]) {
    if (fact.kind === "symbol")
      records.symbols.push({
        ...fact.record,
        declarations: fact.record.declarations.map((x) =>
          x.kind === "external"
            ? x
            : { ...internal(x.declarationRef), revisionId: x.revisionId },
        ),
      });
    if (fact.kind === "typeRelationship")
      records.typeRelationships.push({
        kind: fact.relationshipKind,
        source: {
          ...internal(fact.source.declarationRef),
          syntaxId: idFor(fact.source.declarationRef),
          revisionId: fact.source.revisionId,
        },
        target:
          fact.target.kind === "external"
            ? fact.target
            : {
                ...internal(
                  fact.target.declarationRef === "absent"
                    ? "Parent"
                    : fact.target.declarationRef,
                ),
                syntaxId: idFor(
                  fact.target.declarationRef === "absent"
                    ? "Parent"
                    : fact.target.declarationRef,
                ),
                revisionId: fact.target.revisionId,
              },
        provenanceId: fact.provenanceRef,
      });
  }
  const anchor = {
    document,
    revisionId: "r1",
    contentHash: sha(text),
    range: range(
      nativeByRef.get("Child").nameRange.start,
      nativeByRef.get("Child").nameRange.end,
    ),
    kind: "declarationName",
  };
  records.declarationBindings.push({
    syntaxId: idOf("Child"),
    symbols: binding.record.symbols,
    join: {
      anchor,
      status: "exact",
      candidateIds: [idOf("Child")],
      diagnostic: null,
    },
    provenanceId: "proof-binding",
  });
  for (const field of ["symbols", "declarationBindings", "typeRelationships"])
    rowOrder(records[field]);
  change.records?.(records);
  const C = checkCoverage(loaded, records),
    M = checkMeasurement(loaded, records),
    J = checkJoins(loaded, records, C, M);
  if (change.nonExact) {
    records.declarationBindings[0].syntaxId = null;
    records.declarationBindings[0].join = J.joined.get("binding").join;
  }
  return { loaded, records, C, M, J };
}
const check = ({ loaded, records, C, M, J }) =>
  checkRelationships(loaded, records, C, M, J);
test("captured extends, implements and overrides; explicit external targets and exact declaration-name binding", async (t) => {
  const value = await specimen(t),
    S = check(value);
  assert.deepEqual(S.typeRelationships.map((x) => x.kind).sort(), [
    "extends",
    "implements",
    "overrides",
  ]);
  assert.equal(
    canon(S.recordByFactRef.get("symbol").declarations[1]),
    canon(external),
  );
  assert.equal(S.recordByFactRef.get("binding").syntaxId, idOf("Child"));
  assert.equal(S.recordByFactRef.size, 5);
});
const controls = registerControls([
  ...[
    [
      "SYMBOL.FACT",
      "symbols",
      (r) => (r.symbols[0].displayName = "Changed"),
      "SYMBOL.FACT",
      "symbols",
    ],
    [
      "SYMBOL.SCOPE",
      "symbols",
      (r) => (r.symbols[0].key.document = document),
      "SYMBOL.SCOPE",
      "key",
    ],
    [
      "SYMBOL.FACT.key",
      "symbols",
      (r) => (r.symbols[0].key.symbol = "other Child"),
      "SYMBOL.FACT",
      "symbols",
    ],
    [
      "SYMBOL.TARGET.sourceSet",
      "symbols",
      (r) =>
        (r.symbols[0].declarations[0].document = {
          ...document,
          sourceSetId: "other",
        }),
      "SYMBOL.TARGET",
      "declarations",
    ],
    [
      "SYMBOL.TARGET.duplicate",
      "symbols",
      (r) =>
        r.symbols[0].declarations.push(
          structuredClone(r.symbols[0].declarations[0]),
        ),
      "SYMBOL.TARGET",
      "declarations",
    ],
    [
      "SYMBOL.TARGET",
      "symbols",
      (r) => (r.symbols[0].declarations[0] = internal("Parent")),
      "SYMBOL.TARGET",
      "declarations",
    ],
    [
      "DECLARATION_BINDING.JOIN",
      "declarationBindings",
      (r) => (r.declarationBindings[0].syntaxId = idOf("Parent")),
      "DECLARATION_BINDING.JOIN",
      "join",
    ],
    [
      "DECLARATION_BINDING.JOIN.symbols",
      "declarationBindings",
      (r) =>
        (r.declarationBindings[0].symbols[0] = {
          ...globalKey,
          symbol: "pkg Other",
        }),
      "DECLARATION_BINDING.JOIN",
      "join",
    ],
    [
      "DECLARATION_BINDING.JOIN.family",
      "declarationBindings",
      (r) => (r.declarationBindings[0].join.anchor.kind = "callee"),
      "DECLARATION_BINDING.JOIN",
      "join",
    ],
    [
      "DECLARATION_BINDING.JOIN.tuple",
      "declarationBindings",
      (r) =>
        (r.declarationBindings[0].join.anchor.contentHash =
          sha("different source")),
      "DECLARATION_BINDING.JOIN",
      "join",
    ],
    [
      "DECLARATION_BINDING.PROOF",
      "declarationBindings",
      (r) => (r.declarationBindings[0].provenanceId = "proof-symbol"),
      "DECLARATION_BINDING.PROOF",
      "provenanceId",
    ],
    [
      "RELATIONSHIP.KIND",
      "typeRelationships",
      (r) => (r.typeRelationships[0].kind = "implements"),
      "RELATIONSHIP.KIND",
      "kind",
    ],
    [
      "RELATIONSHIP.SOURCE",
      "typeRelationships",
      (r) => (r.typeRelationships[0].source = internal("Parent")),
      "RELATIONSHIP.SOURCE",
      "source",
    ],
    [
      "RELATIONSHIP.TARGET",
      "typeRelationships",
      (r) => (r.typeRelationships[0].target = internal("Face")),
      "RELATIONSHIP.TARGET",
      "target",
    ],
    [
      "RELATIONSHIP.PROOF",
      "typeRelationships",
      (r) => (r.typeRelationships[0].provenanceId = "proof-binding"),
      "RELATIONSHIP.PROOF",
      "provenanceId",
    ],
    [
      "RELATIONSHIP.TARGET.document",
      "typeRelationships",
      (r) =>
        (r.typeRelationships[0].target.document = {
          ...document,
          path: "src/other.js",
        }),
      "RELATIONSHIP.TARGET",
      "target",
    ],
    [
      "RELATIONSHIP.TARGET.sourceSet",
      "typeRelationships",
      (r) =>
        (r.typeRelationships[0].target.document = {
          ...document,
          sourceSetId: "other",
        }),
      "RELATIONSHIP.TARGET",
      "target",
    ],
    [
      "RELATIONSHIP.TARGET.revision.output",
      "typeRelationships",
      (r) => (r.typeRelationships[0].target.revisionId = "r2"),
      "RELATIONSHIP.TARGET",
      "target",
    ],
    [
      "RECORDS.MEMBERSHIP.missing",
      "typeRelationships",
      (r) => r.typeRelationships.pop(),
      "RECORDS.MEMBERSHIP",
      "typeRelationships",
    ],
    [
      "RECORDS.MEMBERSHIP.extra",
      "symbols",
      (r) => r.symbols.push({ ...r.symbols[0], displayName: "extra" }),
      "RECORDS.MEMBERSHIP",
      "symbols",
    ],
    [
      "RECORDS.MEMBERSHIP.duplicate",
      "symbols",
      (r) => r.symbols.push(structuredClone(r.symbols[0])),
      "RECORDS.MEMBERSHIP",
      "symbols",
    ],
    [
      "RECORDS.ORDER",
      "typeRelationships",
      (r) => r.typeRelationships.reverse(),
      "RECORDS.ORDER",
      "typeRelationships",
    ],
  ].map(([id, field, mutate, expectedAssertion, expectedField]) => ({
    id,
    baseline: () => ({}),
    mutate: (v) => {
      v.change = { records: mutate };
      return v;
    },
    check: async () => true,
    expectedAssertion,
    expectedCode: "invalidRecord",
    expectedField,
  })),
]);
for (const row of controls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => check(await specimen(t, value.change)),
    }),
  );

test("unlocated symbol and non-exact declaration-name diagnostic do not install a declaration", async (t) => {
  const unlocated = await specimen(t, {
    raw: (facts) => (facts[0].record.declarations = []),
  });
  assert.deepEqual(check(unlocated).symbols[0].declarations, []);
  const unmatched = await specimen(t, {
    raw: (facts) => {
      facts[1].anchor.range = span(7, 11);
    },
    nonExact: true,
  });
  assert.equal(check(unmatched).declarationBindings[0].syntaxId, null);
  assert.equal(
    check(unmatched).declarationBindings[0].join.status,
    "unmatched",
  );
});
test("source inheritance alone never creates an uncaptured relationship", async (t) => {
  const value = await specimen(t, { raw: (facts) => facts.splice(2) });
  assert.deepEqual(check(value).typeRelationships, []);
});
test("explicit external relationship target and document-scoped key remain source facts", async (t) => {
  const value = await specimen(t, {
    source: source.replace(
      "class Child extends Parent",
      "class Child extends External",
    ),
    raw: (facts) => {
      facts[0].record.key = {
        scheme: "scip",
        symbol: "src/Main.java Child",
        scope: "document",
        document,
      };
      facts[2] = {
        kind: "typeRelationship",
        ref: "external-base",
        relationshipKind: "extends",
        source: targetRef("Child"),
        target: external,
        provenanceRef: "proof-external",
      };
    },
  });
  const checked = check(value);
  assert.equal(checked.typeRelationships.length, 3);
  assert.equal(
    canon(checked.recordByFactRef.get("external-base").target),
    canon(external),
  );
  assert.equal(checked.symbols[0].key.scope, "document");
});

test("Java mixed bases take each relationship kind from its captured proof", async (t) => {
  const mixed = source.replace(
    "class Child extends Parent {}",
    "class Child extends Parent implements Face {}",
  );
  const value = await specimen(t, {
    source: mixed,
    raw: (facts) =>
      facts.push({
        kind: "typeRelationship",
        ref: "child-face",
        relationshipKind: "implements",
        source: targetRef("Child"),
        target: targetRef("Face"),
        provenanceRef: "proof-child-face",
      }),
  });
  const checked = check(value);
  assert.equal(checked.recordByFactRef.get("extends").kind, "extends");
  assert.equal(checked.recordByFactRef.get("child-face").kind, "implements");
});
test("semantic kind comes from captured proof, not Java heritage spelling", async (t) => {
  const value = await specimen(t, {
    source: source.replace(
      "class Child extends Parent {}",
      "class Child extends Parent implements Face {}",
    ),
    raw: (facts) =>
      facts.push({
        kind: "typeRelationship",
        ref: "child-face",
        relationshipKind: "extends",
        source: targetRef("Child"),
        target: targetRef("Face"),
        provenanceRef: "proof-child-face",
      }),
  });
  assert.equal(check(value).recordByFactRef.get("child-face").kind, "extends");
});
test("Java override marker and literal parameter spelling do not control semantic proof", async (t) => {
  for (const text of [
    source.replace("@Override ", ""),
    source.replace(
      "@Override void method()",
      "@Override void method(int value)",
    ),
  ]) {
    const value = await specimen(t, { source: text });
    assert.equal(
      check(value).recordByFactRef.get("overrides").kind,
      "overrides",
    );
  }
});
test("opaque external relationship keeps independent source provenance", async (t) => {
  const value = await specimen(t, {
    source: source.replace(
      "class Child extends Parent",
      "class Child extends External",
    ),
    raw: (facts) => {
      facts[2].target = external;
    },
    externalReference: external,
  });
  assert.equal(
    canon(check(value).recordByFactRef.get("extends").target),
    canon(external),
  );
  assert.equal(value.J.joined.get("base-reference").join.status, "exact");
});
const historicalControls = registerControls([
  {
    id: "RELATIONSHIP.TARGET.admittedHistorical",
    baseline: () => ({ history: true }),
    mutate: (value) => ({ ...value, historicalTarget: true }),
    check: async () => true,
    expectedAssertion: "RELATIONSHIP.TARGET",
    expectedCode: "invalidRecord",
    expectedField: "target",
  },
  {
    id: "SYMBOL.TARGET.admittedHistorical",
    baseline: () => ({ history: true }),
    mutate: (value) => ({ ...value, historicalSymbol: true }),
    check: async () => true,
    expectedAssertion: "SYMBOL.TARGET",
    expectedCode: "invalidRecord",
    expectedField: "declarations",
  },
]);
for (const row of historicalControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => {
        const item = await specimen(t, value);
        const r2 = item.loaded.revisions.get(JSON.stringify(["main", "r2"]));
        assert.equal(
          r2.documents[0].contentHash,
          sha("// r2 source\n" + source),
        );
        assert.equal(
          item.loaded.native.declarations.filter((d) => d.revisionId === "r2")
            .length,
          declarations.length,
        );
        assert.equal(
          item.records.provenance.find((p) => p.id === "proof-r2-extends")
            .freshness,
          "fresh",
        );
        assert.equal(
          item.records.provenance.find((p) => p.id === "proof-r2-symbol")
            .freshness,
          "fresh",
        );
        assert.equal(
          item.records.provenance.find((p) => p.id === "proof-extends")
            .freshness,
          "stale",
        );
        return check(item);
      },
    }),
  );
const nonExactControls = registerControls([
  {
    id: "DECLARATION_BINDING.JOIN.nonExactSyntaxId",
    baseline: () => ({}),
    mutate: (value) => ({ ...value, mutated: true }),
    check: async () => true,
    expectedAssertion: "DECLARATION_BINDING.JOIN",
    expectedCode: "invalidRecord",
    expectedField: "join",
  },
]);
for (const row of nonExactControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => {
        const item = await specimen(t, {
          raw: (facts) => {
            facts[1].anchor.range = span(7, 11);
          },
          nonExact: true,
        });
        if (value.mutated)
          item.records.declarationBindings[0].syntaxId = idOf("Child");
        return check(item);
      },
    }),
  );
const sourceControls = registerControls(
  [
    [
      "SYMBOL.SCOPE.raw",
      (facts) =>
        (facts[0].record.key = {
          ...globalKey,
          scope: "document",
          document: { ...document, path: "src/not-here.js" },
        }),
      "SYMBOL.SCOPE",
      "key",
    ],
    [
      "SYMBOL.TARGET.duplicate.raw",
      (facts) =>
        facts[0].record.declarations.push(
          structuredClone(facts[0].record.declarations[0]),
        ),
      "SYMBOL.TARGET",
      "declarations",
    ],
    [
      "SYMBOL.FACT.conflict.raw",
      (facts) =>
        facts.push({
          ...structuredClone(facts[0]),
          ref: "symbol-conflict",
          record: {
            ...structuredClone(facts[0].record),
            displayName: "Other Child",
            provenanceId: "proof-conflict",
          },
        }),
      "SYMBOL.FACT",
      "symbols",
    ],
    [
      "SYMBOL.TARGET.raw",
      (facts) => (facts[0].record.declarations[0].revisionId = "r2"),
      "SYMBOL.TARGET",
      "declarations",
    ],
    [
      "DECLARATION_BINDING.JOIN.raw",
      (facts) => (facts[1].record.symbols = []),
      "DECLARATION_BINDING.JOIN",
      "symbols",
    ],
    [
      "DECLARATION_BINDING.JOIN.family.raw",
      (facts) => (facts[1].anchor.kind = "callee"),
      "JOIN.FAMILY",
      "anchor.kind",
    ],
    [
      "DECLARATION_BINDING.JOIN.tuple.raw",
      (facts) => (facts[1].anchor.range = span(7, 11)),
      "DECLARATION_BINDING.JOIN",
      "join",
    ],
    [
      "RELATIONSHIP.SOURCE.raw",
      (facts) => {
        facts[2].source.declarationRef = "Derived.method";
        facts[2].target.declarationRef = "Parent";
      },
      "RELATIONSHIP.SOURCE",
      "source",
    ],
    [
      "RELATIONSHIP.TARGET.raw",
      (facts) => (facts[2].target.declarationRef = "absent"),
      "RELATIONSHIP.TARGET",
      "target",
    ],
    [
      "RELATIONSHIP.TARGET.revision",
      (facts) => (facts[2].target.revisionId = "r2"),
      "RELATIONSHIP.TARGET",
      "target",
    ],
  ].map(([id, raw, expectedAssertion, expectedField]) => ({
    id,
    baseline: () => ({}),
    mutate: (value) => ({ ...value, change: { raw } }),
    check: async () => true,
    expectedAssertion,
    expectedCode: "invalidRecord",
    expectedField,
  })),
);
for (const row of sourceControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => check(await specimen(t, value.change)),
    }),
  );

const rawSymbolControls = registerControls(
  [
    [
      "SYMBOL.FACT.key.raw",
      (facts) => {
        facts[0].record.key.symbol = "another Child";
      },
      (records) => {
        records.symbols[0].key = structuredClone(globalKey);
      },
    ],
    [
      "SYMBOL.FACT.display.raw",
      (facts) => {
        facts[0].record.displayName = "Different Child";
      },
      (records) => {
        records.symbols[0].displayName = "Child";
      },
    ],
  ].map(([id, raw, records]) => ({
    id,
    baseline: () => ({}),
    mutate: (value) => ({ ...value, change: { raw, records } }),
    check: async () => true,
    expectedAssertion: "SYMBOL.FACT",
    expectedCode: "invalidRecord",
    expectedField: "symbols",
  })),
);
for (const row of rawSymbolControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => check(await specimen(t, value.change)),
    }),
  );
const shapeControls = registerControls(
  [
    [
      "missing",
      (fact) => {
        delete fact.relationshipKind;
      },
      "SemanticCapture.facts[2].relationshipKind",
    ],
    [
      "default",
      (fact) => {
        fact.relationshipKind = null;
      },
      "SemanticCapture.facts[2].relationshipKind",
    ],
    [
      "discriminator-substituted",
      (fact) => {
        fact.kind = "extends";
        delete fact.relationshipKind;
      },
      "SemanticCapture.facts[2].kind",
    ],
    [
      "external-source",
      (fact) => {
        fact.source = external;
      },
      "SemanticCapture.facts[2].source.kind",
    ],
  ].map(([label, raw, expectedField]) => ({
    id: `RELATIONSHIP.KIND.shape.${label}`,
    baseline: () => ({}),
    mutate: (value) => ({
      ...value,
      change: { raw: (facts) => raw(facts[2]) },
    }),
    check: async () => true,
    expectedAssertion: "FORMAT.SHAPE",
    expectedCode: "invalidRecord",
    expectedField,
  })),
);
for (const row of shapeControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => check(await specimen(t, value.change)),
    }),
  );

const proofControls = registerControls(
  [
    ["SYMBOL.FACT.evidenceKind", 0, "semanticReference", "IDENTITY.SEMANTIC"],
    [
      "DECLARATION_BINDING.PROOF.evidenceKind",
      1,
      "semanticReference",
      "IDENTITY.SEMANTIC",
    ],
    [
      "RELATIONSHIP.PROOF.evidenceKind",
      2,
      "declarationBinding",
      "IDENTITY.SEMANTIC",
    ],
  ].map(([id, index, evidenceKind, expectedAssertion]) => ({
    id,
    baseline: () => ({}),
    mutate: (value) => ({
      ...value,
      change: {
        proofs: (proofs) => {
          proofs[index].evidenceKind = evidenceKind;
        },
      },
    }),
    check: async () => true,
    expectedAssertion,
    expectedCode: "invalidRecord",
    expectedField:
      expectedAssertion === "IDENTITY.SEMANTIC"
        ? ["symbol", "binding", "extends"][index]
        : "provenanceId",
  })),
);
for (const row of proofControls)
  test(row.id, async (t) =>
    runControl({
      ...row,
      check: async (value) => check(await specimen(t, value.change)),
    }),
  );

async function inheritanceSpecimen(
  t,
  variant = "valid",
  language = "rust",
  withFacts = true,
  crossFile = false,
) {
  const root = await mkdtemp(join(tmpdir(), `relationships-${language}-`));
  t.after(() => rm(root, { recursive: true, force: true }));
  const parentText =
    language === "python"
      ? "class Parent:\n    pass\n"
      : language === "java"
        ? variant === "override"
          ? "class Parent { void method() {} }\n"
          : "class Parent<T> {}\n"
        : "trait Parent {}\n";
  const childText =
    language === "python"
      ? `class Child${variant === "no-supertrait" ? "" : variant === "false-base" ? "(Face)" : variant === "generic" ? "(Parent[int])" : "(Parent)"}:
    pass
`
      : language === "java"
        ? variant === "override"
          ? `class Child extends Parent { void method() {} }
`
          : `class Child extends Parent<String> {}
`
        : `trait Child${variant === "no-supertrait" ? "" : variant === "generic" ? "<T>: Parent + Face" : ": Parent"} {}
trait Face {}
struct Impl;
impl${variant === "generic" ? "<T>" : ""} ${variant === "inherent" ? "" : "Face for "}Impl${variant === "generic" ? "<T>" : ""} {}
`;
  const sourceSetId = `${language}-main`,
    text = crossFile ? childText : parentText + childText;
  const doc = {
    sourceSetId,
    language,
    path:
      language === "python"
        ? "src/main.py"
        : language === "java"
          ? "src/Main.java"
          : "src/lib.rs",
  };
  const targetDoc = {
    ...doc,
    path:
      language === "python"
        ? "src/base.py"
        : language === "java"
          ? "src/Base.java"
          : "src/base.rs",
  };
  const specs =
    language === "java"
      ? variant === "override"
        ? [
            ["Parent", "type", "class Parent", [], []],
            ["Child", "type", "class Child", [], ["Parent"]],
            ["Parent.method", "method", "void method", [], [], "Parent"],
            ["Child.method", "method", "void method", [], [], "Child"],
          ]
        : [
            ["Parent", "type", "class Parent", ["T"], []],
            ["Child", "type", "class Child", [], ["Parent<String>"]],
          ]
      : language === "python"
        ? [
            ["Parent", "type", "class Parent", [], []],
            [
              "Child",
              "type",
              "class Child",
              [],
              variant === "no-supertrait"
                ? []
                : [
                    variant === "false-base"
                      ? "Face"
                      : variant === "generic"
                        ? "Parent[int]"
                        : "Parent",
                  ],
            ],
          ]
        : [
            ["Parent", "type", "trait Parent", [], []],
            [
              "Child",
              "type",
              "trait Child",
              variant === "generic" ? ["T"] : [],
              variant === "no-supertrait" ? [] : ["Parent"],
            ],
            ["Face", "type", "trait Face", [], []],
            ["Impl", "type", "struct Impl", [], []],
            [
              "FaceImpl",
              "implementation",
              variant === "generic" ? "impl<T>" : "impl ",
              variant === "generic" ? ["T"] : [],
              variant === "inherent" ? [] : ["Face"],
            ],
          ];
  const rows = specs.map(
    ([ref, kind, head, typeParameters, bases, parentRef = null]) => {
      const rowText =
        crossFile && (ref === "Parent" || ref === "Parent.method")
          ? parentText
          : text;
      const rowDoc =
        crossFile && (ref === "Parent" || ref === "Parent.method")
          ? targetDoc
          : doc;
      const start = ref.endsWith(".method")
        ? rowText.indexOf(head, rowText.indexOf(`class ${parentRef}`))
        : rowText.indexOf(head);
      const end =
        language === "python"
          ? rowText.indexOf("pass", start) + 4
          : rowText.indexOf(ref === "Impl" ? ";" : "}", start) + 1;
      const name = ref.endsWith(".method")
        ? "method"
        : ref === "FaceImpl"
          ? "Impl"
          : ref;
      const nameStart =
        ref === "FaceImpl"
          ? rowText.indexOf("Impl", start)
          : rowText.indexOf(name, start);
      const header = {
        kind,
        name,
        modifiers: [],
        typeParameters,
        parameters: [],
        resultType: null,
        bases,
      };
      const witnesses = [
        witness("name", nameStart, nameStart + name.length, name),
        witness("header.name", nameStart, nameStart + name.length, name),
        ...typeParameters.map((value, i) => {
          const at = rowText.indexOf(
            value,
            ref === "FaceImpl" ? start : nameStart + name.length,
          );
          return witness(
            `header.typeParameters[${i}]`,
            at,
            at + value.length,
            value,
          );
        }),
        ...bases.map((base, i) => {
          const at = rowText.indexOf(
            base,
            ref === "FaceImpl" ? start : nameStart + name.length,
          );
          return witness(`header.bases[${i}]`, at, at + base.length, base);
        }),
      ];
      return {
        ref,
        nativeId: null,
        role: "production",
        document: rowDoc,
        revisionId: "r1",
        parentRef,
        kind,
        name,
        range: span(start, end),
        nameRange: span(nameStart, nameStart + name.length),
        header,
        signature: null,
        witnesses,
      };
    },
  );
  const key = (row) => ({
    kind: row.kind,
    name: row.name,
    signature: null,
    ordinal: 0,
  });
  const ids = new Map(
    rows.map((row) => [
      row.ref,
      `sid:v1:${digest("syntax", { sourceSet: sourceSetId, path: row.document.path, language, ancestors: row.parentRef ? [key(rows.find((parent) => parent.ref === row.parentRef))] : [], declaration: key(row) })}`,
    ]),
  );
  const ref = (name) => ({
    kind: "internal",
    declarationRef: name,
    revisionId: "r1",
  });
  const toTarget = (name) => ({
    kind: "internal",
    syntaxId: ids.get(name),
    document: rows.find((row) => row.ref === name).document,
    revisionId: "r1",
  });
  const facts = [
    {
      kind: "typeRelationship",
      ref: "rust-extends",
      relationshipKind: "extends",
      source: ref("Child"),
      target: ref("Parent"),
      provenanceRef: "proof-rust-extends",
    },
    ...(variant === "override" && language === "java"
      ? [
          {
            kind: "typeRelationship",
            ref: "java-override",
            relationshipKind: "overrides",
            source: ref("Child.method"),
            target: ref("Parent.method"),
            provenanceRef: "proof-java-override",
          },
        ]
      : []),
    ...(language !== "rust"
      ? []
      : [
          {
            kind: "typeRelationship",
            ref: "rust-implements",
            relationshipKind: "implements",
            source: ref("FaceImpl"),
            target: ref("Face"),
            provenanceRef: "proof-rust-implements",
          },
        ]),
  ];
  if (!withFacts) facts.length = 0;
  const files = new Map(),
    put = (path, obj) =>
      files.set(path, typeof obj === "string" ? obj : JSON.stringify(obj));
  const snapshot = `snapshots/${doc.path}`,
    targetSnapshot = `snapshots/${targetDoc.path}`;
  put(snapshot, text);
  if (crossFile) put(targetSnapshot, parentText);
  put("captures/native.json", {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    declarations: rows,
    calls: [],
    controls: [],
    references: [],
  });
  put("captures/semantic.json", {
    formatVersion: 1,
    producerId: "semantic",
    facts,
  });
  for (const [name, value] of [
    ["native", "native executable"],
    ["semantic", "semantic executable"],
    ["toolchain", "toolchain"],
    ["config", "config"],
    ["dependency", "dependency"],
  ])
    put(`captures/${name}.txt`, value);
  const captures = [
    ["native", "executable", "captures/native.txt"],
    ["semantic", "executable", "captures/semantic.txt"],
    ["toolchain", "toolchain", "captures/toolchain.txt"],
    ["config", "config", "captures/config.txt"],
    ["dependency", "dependency", "captures/dependency.txt"],
    ["artifact", "semanticArtifact", "captures/semantic.json"],
  ].map(([ref, kind, file]) => ({
    ref,
    kind,
    file,
    hash: sha(files.get(file)),
  }));
  const producers = [nativeProducer, semanticProducer].map((row) => ({
    ...row,
    languages: [language],
  }));
  const documents = [
    { key: doc, revisionId: "r1", sourceFile: snapshot },
    ...(crossFile
      ? [{ key: targetDoc, revisionId: "r1", sourceFile: targetSnapshot }]
      : []),
  ].sort((a, b) =>
    Buffer.compare(Buffer.from(a.key.path), Buffer.from(b.key.path)),
  );
  const revision = {
    id: "r1",
    sourceSetId,
    documents,
    toolchainHash: captures[2].hash,
    configHash: captures[3].hash,
    dependencyHash: captures[4].hash,
  };
  const sourceRows = documents.map((row) => ({
    document: row.key,
    contentHash: sha(row.key.path === doc.path ? text : parentText),
  }));
  const basis = {
    producerId: "semantic",
    producerVersion: "1",
    producerHash: semanticProducer.executableHash,
    artifactHash: captures[5].hash,
    language,
    sourceSetId,
    revisionId: "r1",
    sourceManifestHash: sha(canon(sourceRows)),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const proofs = facts.map((fact) => ({
    id: fact.provenanceRef,
    producerId: "semantic",
    document: doc,
    revisionId: "r1",
    contentHash: sha(text),
    evidenceKind: "typeRelationship",
    basis,
    freshness: "fresh",
  }));
  const coverage = (id, d = doc) => ({
    producerId: id,
    language,
    sourceSetId,
    documentPath: d.path,
    revisionId: "r1",
    requested: true,
    selected: true,
    state: "complete",
    supportedRoles: ["definition"],
    observedRoles: ["definition"],
    diagnostic: null,
  });
  put(`${snapshot}.annotations.json`, {
    formatVersion: 1,
    document: doc,
    revisionId: "r1",
    scenarios: [],
    facts: [
      ...["native", "semantic"].map((id) => ({
        kind: "coverage",
        ref: `coverage-${id}`,
        record: coverage(id),
      })),
      ...proofs.map((record, i) => ({
        kind: "provenance",
        ref: `provenance-${i}`,
        record,
      })),
      ...facts,
    ],
  });
  if (crossFile)
    put(`${targetSnapshot}.annotations.json`, {
      formatVersion: 1,
      document: targetDoc,
      revisionId: "r1",
      scenarios: [],
      facts: ["native", "semantic"].map((id) => ({
        kind: "coverage",
        ref: `base-coverage-${id}`,
        record: coverage(id, targetDoc),
      })),
    });
  const fixture = {
    formatVersion: 1,
    profile: "example",
    language,
    sourceSets: [
      {
        id: sourceSetId,
        rootId: "root",
        languages: [language],
        dependencies: [],
      },
    ],
    producers,
    revisions: [revision],
    comparison: { sourceSetId, revisionId: "r1", producers },
    coverageIntents: documents.flatMap(({ key }) =>
      producers.map((p) => ({
        producerId: p.id,
        document: key,
        revisionId: "r1",
        requestedRoles: ["definition"],
        measurementSupport: [
          "declarationName",
          "callee",
          "invocation",
          "reference",
        ].map((kind) => ({ kind, available: true, diagnostic: null })),
      })),
    ),
    nativeArtifact: "captures/native.json",
    semanticArtifacts: ["captures/semantic.json"],
    annotationFiles: [
      `${snapshot}.annotations.json`,
      ...(crossFile ? [`${targetSnapshot}.annotations.json`] : []),
    ],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  for (const [name, contents] of files) {
    await mkdir(dirname(join(root, name)), { recursive: true });
    await writeFile(join(root, name), contents);
  }
  const loaded = await loadFixture(root);
  const normalized = rows.map((d) => ({
    syntaxId: ids.get(d.ref),
    role: "production",
    document: d.document,
    revisionId: "r1",
    kind: d.kind,
    name: d.name,
    lookupKey: d.name,
    ancestors: d.parentRef
      ? [key(rows.find((parent) => parent.ref === d.parentRef))]
      : [],
    key: key(d),
    range: range(d.range.start, d.range.end),
    nameRange: range(d.nameRange.start, d.nameRange.end),
    header: d.header,
    provenanceId: `native:r1:${ids.get(d.ref)}`,
  }));
  const records = {
    formatVersion: 1,
    comparison: fixture.comparison,
    producers: structuredClone(producers).sort((a, b) =>
      Buffer.compare(Buffer.from(canon(a)), Buffer.from(canon(b))),
    ),
    sourceSets: fixture.sourceSets,
    revisions: [
      {
        ...revision,
        documents: documents.map((d) => ({
          key: d.key,
          revisionId: "r1",
          contentHash: sha(d.key.path === doc.path ? text : parentText),
          byteLength: Buffer.byteLength(
            d.key.path === doc.path ? text : parentText,
          ),
        })),
      },
    ],
    coverage: rowOrder(
      documents.flatMap(({ key }) =>
        ["native", "semantic"].map((id) => coverage(id, key)),
      ),
    ),
    provenance: [
      ...proofs,
      ...rows.map((d) => ({
        id: `native:r1:${ids.get(d.ref)}`,
        producerId: "native",
        document: d.document,
        revisionId: "r1",
        contentHash: sha(d.document.path === doc.path ? text : parentText),
        evidenceKind: "measuredSyntax",
        basis: null,
        freshness: "fresh",
      })),
    ],
    declarations: rowOrder(normalized),
    symbols: [],
    declarationBindings: [],
    typeRelationships: rowOrder(
      facts.map((fact) => ({
        kind: fact.relationshipKind,
        source: toTarget(fact.source.declarationRef),
        target: toTarget(fact.target.declarationRef),
        provenanceId: fact.provenanceRef,
      })),
    ),
    calls: [],
    controlRegions: [],
    references: [],
    referenceJoinDiagnostics: [],
    callBindings: [],
    durableAnchors: [],
    groupContinuities: [],
    anchorResults: [],
  };
  const C = checkCoverage(loaded, records),
    M = checkMeasurement(loaded, records),
    J = checkJoins(loaded, records, C, M);
  return { loaded, records, C, M, J };
}
test("Rust source has measured supertrait and trait implementation direction", async (t) => {
  const checked = check(await inheritanceSpecimen(t));
  assert.deepEqual(checked.typeRelationships.map((row) => row.kind).sort(), [
    "extends",
    "implements",
  ]);
});
test("Python measured parenthesized inheritance needs an explicit extends fact", async (t) => {
  const checked = check(await inheritanceSpecimen(t, "valid", "python"));
  assert.deepEqual(
    checked.typeRelationships.map((row) => row.kind),
    ["extends"],
  );
  const syntaxOnly = check(
    await inheritanceSpecimen(t, "valid", "python", false),
  );
  assert.deepEqual(syntaxOnly.typeRelationships, []);
});
test("Rust and Python semantics do not depend on a header base projection", async (t) => {
  for (const language of ["rust", "python"])
    for (const variant of [
      "no-supertrait",
      "false-base",
      ...(language === "rust" ? ["inherent"] : []),
    ]) {
      const checked = check(await inheritanceSpecimen(t, variant, language));
      assert.equal(
        checked.typeRelationships.some((row) => row.kind === "extends"),
        true,
      );
    }
});

test("source-backed cross-document generic inheritance has exact measured target IDs in Java, Rust and Python", async (t) => {
  for (const language of ["java", "rust", "python"]) {
    const value = await inheritanceSpecimen(t, "generic", language, true, true);
    const checked = check(value);
    const relation = checked.recordByFactRef.get("rust-extends");
    assert.equal(relation.kind, "extends");
    assert.notEqual(
      relation.source.document.path,
      relation.target.document.path,
    );
    assert.equal(
      relation.target.syntaxId,
      value.M.recordByNativeRef.get("Parent").syntaxId,
    );
    assert.equal(checked.typeRelationships.length, language === "rust" ? 2 : 1);
  }
});
test("Java measured override crosses source documents without an optional annotation", async (t) => {
  const value = await inheritanceSpecimen(t, "override", "java", true, true);
  const relation = check(value).recordByFactRef.get("java-override");
  assert.equal(relation.kind, "overrides");
  assert.notEqual(relation.source.document.path, relation.target.document.path);
  assert.equal(
    relation.target.syntaxId,
    value.M.recordByNativeRef.get("Parent.method").syntaxId,
  );
});
test("cross-file relation needs captured independent semantic proof, not syntax similarity", async (t) => {
  const noFacts = await inheritanceSpecimen(t, "generic", "java", false, true);
  assert.deepEqual(check(noFacts).typeRelationships, []);
  const forged = await inheritanceSpecimen(t, "generic", "java", true, true);
  forged.records.typeRelationships[0].provenanceId = "not-a-captured-proof";
  assert.throws(
    () => check(forged),
    (error) =>
      error.assertion === "RELATIONSHIP.PROOF" &&
      error.field === "provenanceId",
  );
  const unmeasured = await inheritanceSpecimen(
    t,
    "generic",
    "python",
    true,
    true,
  );
  unmeasured.loaded.native.declarations =
    unmeasured.loaded.native.declarations.filter((row) => row.ref !== "Parent");
  assert.throws(
    () => check(unmeasured),
    (error) =>
      error.assertion === "RELATIONSHIP.TARGET" && error.field === "target",
  );
});
