import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { join, dirname } from "node:path";
import { tmpdir } from "node:os";
import { loadFixture } from "../load.mjs";
import { checkMeasurement } from "../record-check/measurement.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import { checkJoins } from "../record-check/joins.mjs";
import { checkCounts, assertCorpusFloors } from "../counts.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { registerControls, runControl } from "./mutations.mjs";

// Independently authored source offsets, digest inputs and expected normalized rows.
const source =
  "function main() { target(); callback; }\nfunction target() {}\n";
const alternateSource = (kind) => `${source}// admitted ${kind} snapshot\n`;
const baseDocument = {
  sourceSetId: "main",
  language: "javascript",
  path: "src/main.js",
};
const document = baseDocument;
const sha = (value) => createHash("sha256").update(value).digest("hex");
const canonical = (value) =>
  value === null || typeof value !== "object"
    ? JSON.stringify(value)
    : Array.isArray(value)
      ? "[" + value.map(canonical).join(",") + "]"
      : "{" +
        Object.keys(value)
          .sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)))
          .map((k) => canonical(k) + ":" + canonical(value[k]))
          .join(",") +
        "}";
const domain = (name, input) =>
  createHash("sha256")
    .update(`trellis.${name}.v1\0`)
    .update(canonical(input))
    .digest("hex")
    .slice(0, 32);
const span = (start, end, encoding = "utf8") => ({ start, end, encoding });
const range = (start, end) => ({ start, end });
const witness = (field, start, end, text) => ({
  field,
  witness: { range: span(start, end), text },
});
const header = (name) => ({
  kind: "function",
  name,
  modifiers: [],
  typeParameters: [],
  parameters: [],
  resultType: null,
  bases: [],
});
const syntax = (name) =>
  `sid:v1:${domain("syntax", { sourceSet: "main", path: document.path, language: "javascript", ancestors: [], declaration: { kind: "function", name, signature: null, ordinal: 0 } })}`;
// Decision 0003: occ:v2 binds document bytes, extraction context and native
// producer descriptor; the declared native inventory is explicitly empty.
const extractionContext = sha(
  "trellis.extraction-context.v1\0" +
    canonical({ language: "javascript", components: [] }),
);
const occurrence = (owner, kind, ordinal, text = source) =>
  sha(
    "trellis.occurrence.v2\0" +
      canonical({
        contentHash: sha(text),
        extractionContext,
        nativeProducerId: "native",
        nativeProducerVersion: "1",
        ownerSyntaxId: owner,
        kind,
        ordinal,
      }),
  ).replace(/^(.{32}).*$/, "occ:v2:$1");
const mainId = syntax("main"),
  targetId = syntax("target");
const callId = occurrence(mainId, "call", 0),
  refId = occurrence(mainId, "reference", 0),
  callbackId = occurrence(mainId, "reference", 1);
const declaration = (ref, name, start, end, nameStart) => ({
  ref,
  nativeId: null,
    role: "production",
  document,
  revisionId: "r1",
  parentRef: null,
  kind: "function",
  name,
  range: span(start, end),
  nameRange: span(nameStart, nameStart + name.length),
  header: header(name),
  signature: null,
  witnesses: [
    witness("name", nameStart, nameStart + name.length, name),
    witness("header.name", nameStart, nameStart + name.length, name),
  ],
});
const native = {
  formatVersion: 1,
  producerId: "native",
  extractionInputs: [],
  declarations: [
    declaration("main", "main", 0, 39, 9),
    declaration("target", "target", 40, 60, 49),
  ],
  calls: [
    {
      ref: "call",
      nativeId: null,
      document,
      revisionId: "r1",
      ownerRef: "main",
      range: span(18, 26),
      calleeRange: span(18, 24),
      spelling: "target",
      regionRefs: [],
      witnesses: [witness("spelling", 18, 24, "target")],
    },
  ],
  controls: [],
  references: [
    {
      ref: "target-ref",
      nativeId: null,
      document,
      revisionId: "r1",
      ownerRef: "main",
      range: span(18, 24),
      spelling: "target",
      witnesses: [witness("spelling", 18, 24, "target")],
    },
    {
      ref: "callback-ref",
      nativeId: null,
      document,
      revisionId: "r1",
      ownerRef: "main",
      range: span(28, 36),
      spelling: "callback",
      witnesses: [witness("spelling", 28, 36, "callback")],
    },
  ],
};
const nativeProducer = {
  id: "native",
  version: "1",
  executableHash: sha("native-executable"),
  kind: "native",
  languages: ["javascript"],
  positionEncoding: "utf8",
};
const semanticProducer = {
  id: "semantic",
  version: "1",
  executableHash: sha("semantic-executable"),
  kind: "semantic",
  languages: ["javascript"],
  positionEncoding: "utf8",
};
const contentHash = sha(source);
const declaredTarget = {
  kind: "internal",
  declarationRef: "target",
  revisionId: "r1",
};
const target = {
  kind: "internal",
  syntaxId: targetId,
  document,
  revisionId: "r1",
};
const templates = {
  declarationBinding: {
    symbols: [
      { scheme: "scip", symbol: "pkg target", scope: "global", document: null },
    ],
    provenanceId: "proof",
  },
  callBinding: {
    resolution: "resolved",
    declaredTarget,
    candidates: [],
    dispatch: "direct",
    possibleDispatch: [],
    possibleDispatchComplete: false,
    provenanceId: "proof",
  },
  reference: {
    site: "use",
    roles: ["read", "call"],
    resolution: "resolved",
    declaredTarget,
    candidates: [],
    provenanceId: "proof",
  },
};
const owned = {
  declarationName: "target",
  callee: "main",
  invocation: "main",
  reference: "main",
};
const exactSpans = {
  declarationName: [49, 55],
  callee: [18, 24],
  invocation: [18, 26],
  reference: [18, 24],
};
const unmatchedSpans = {
  declarationName: [40, 48],
  callee: [28, 36],
  invocation: [28, 36],
  reference: [0, 8],
};
const factType = {
  declarationName: "declarationBinding",
  callee: "callBinding",
  invocation: "callBinding",
  reference: "reference",
};
const ordered = (rows) =>
  rows.sort((a, b) =>
    Buffer.compare(Buffer.from(canonical(a)), Buffer.from(canonical(b))),
  );
const absent = () => ({
  formatVersion: 1,
  comparison: null,
  producers: [],
  sourceSets: [],
  revisions: [],
  coverage: [],
  provenance: [],
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
});
function makeFact(family, status, ref = "fact") {
  const coordinates = (status === "exact" ? exactSpans : unmatchedSpans)[
    family
  ];
  return {
    kind: factType[family],
    ref,
    anchor: {
      document,
      revisionId: "r1",
      contentHash,
      kind: family,
      range: span(...coordinates),
      ownerRef: owned[family],
    },
    record: structuredClone(templates[factType[family]]),
  };
}
async function specimen(
  t,
  {
    family = "reference",
    status = "exact",
    facts = null,
    unsupportedFamily = family,
    encoding = "utf8",
    emojiPrefix = false,
    control = false,
    declarationSite = false,
    callbackSite = false,
    twoProducers = false,
    selectSecond = false,
    mutateFact = null,
    mutateNative = null,
    mutateSupport = null,
    alternateTuples = false,
    normalized = false,
    sourceOverride = null,
    mutateSecond = null,
    secondEncoding = null,
    documentOverride = null,
    profile = "example",
    scenarios = [],
    dispositions = null,
    mutateCoverage = null,
    requestedRoles = ["read"],
    includeSecondInComparison = false,
    comparisonSemanticVersion = null,
  } = {},
) {
  const document = documentOverride ?? baseDocument;
  // The directory name fixes the admitted profile and language.
  const parent = await mkdtemp(join(tmpdir(), "count-u5-")),
    root = join(parent, profile === "example" ? "example" : document.language);
  await mkdir(root);
  t.after(() => rm(parent, { recursive: true, force: true }));
  const fs = new Map(),
    put = (path, value) =>
      fs.set(path, typeof value === "string" ? value : JSON.stringify(value));
  const shifted = emojiPrefix || encoding !== "utf8",
    offset = shifted ? 5 : 0,
    positionOffset =
      encoding === "utf16" ? 3 : encoding === "unicodeScalar" ? 2 : offset;
  const sourceText = shifted
      ? "😀\n" + (sourceOverride ?? source)
      : (sourceOverride ?? source),
    sourceHash = sha(sourceText);
  // Occurrence IDs bind this specimen's admitted main.js bytes (Decision 0003).
  const callId = occurrence(mainId, "call", 0, sourceText),
    refId = occurrence(mainId, "reference", 0, sourceText),
    callbackId = occurrence(mainId, "reference", 1, sourceText);
  const alternateText = (kind) => `${sourceText}// admitted ${kind} snapshot\n`;
  const measuredNative = structuredClone(native);
  if (shifted)
    for (const row of [
      ...measuredNative.declarations,
      ...measuredNative.calls,
      ...measuredNative.references,
    ]) {
      for (const key of ["range", "nameRange", "calleeRange"])
        if (row[key]) {
          row[key].start += offset;
          row[key].end += offset;
        }
      for (const entry of row.witnesses) {
        entry.witness.range.start += offset;
        entry.witness.range.end += offset;
      }
    }
  if (control) {
    measuredNative.controls.push({
      ref: "region",
      nativeId: null,
      document,
      revisionId: "r1",
      ownerRef: "main",
      parentRef: null,
      kind: "if",
      range: span(18 + offset, 26 + offset),
      arm: null,
      witnesses: [],
    });
    measuredNative.calls[0].regionRefs = ["region"];
  }
  if (declarationSite)
    measuredNative.references.push({
      ref: "declaration-ref",
      nativeId: null,
      document,
      revisionId: "r1",
      ownerRef: "target",
      range: span(49 + offset, 55 + offset),
      spelling: "target",
      witnesses: [witness("spelling", 49 + offset, 55 + offset, "target")],
    });
  mutateNative?.(measuredNative);
  put("snapshots/src/main.js", sourceText);
  put("captures/native.json", measuredNative);
  for (const [name, value] of [
    ["native", "native-executable"],
    ["semantic", "semantic-executable"],
    ["toolchain", "toolchain"],
    ["config", "config"],
    ["dependency", "dependency"],
  ])
    put(`captures/${name}.txt`, value);
  for (const scenario of scenarios)
    for (const anchor of scenario.anchors) anchor.contentHash = sourceHash;
  const authored = structuredClone(facts ?? [makeFact(family, status)]);
  for (const fact of authored) {
    if (!fact.anchor) continue;
    fact.anchor.contentHash = sourceHash;
    if (shifted) {
      fact.anchor.range.start += positionOffset;
      fact.anchor.range.end += positionOffset;
      fact.anchor.range.encoding = encoding;
    }
    if (callbackSite && fact.kind === "reference") {
      fact.anchor.range = span(
        28 + positionOffset,
        36 + positionOffset,
        encoding,
      );
      fact.record.roles = ["read"];
    }
    if (
      declarationSite &&
      fact.kind === "reference" &&
      fact.anchor.range.start === 18 + positionOffset
    ) {
      fact.anchor.range = span(
        49 + positionOffset,
        55 + positionOffset,
        encoding,
      );
      fact.anchor.ownerRef = "target";
      fact.record.site = "declaration";
      fact.record.roles = ["definition"];
    }
    mutateFact?.(fact);
  }
  for (let i = 0; i < authored.length; i++) {
    const id = i === 0 ? "proof" : `proof-${authored[i].ref}`;
    if (authored[i].kind === "typeRelationship") authored[i].provenanceRef = id;
    else authored[i].record.provenanceId = id;
  }
  if (twoProducers) {
    const other = structuredClone(authored[0]);
    other.ref = "fact-b";
    if (other.kind === "typeRelationship") other.provenanceRef = "proof-b";
    else other.record.provenanceId = "proof-b";
    mutateSecond?.(other);
    authored.push(other);
  }
  put("captures/semantic.json", {
    formatVersion: 1,
    producerId: "semantic",
    facts: authored.filter((f) => f.ref !== "fact-b"),
  });
  if (twoProducers) {
    put("captures/semantic-b.txt", "semantic-b-executable");
    put("captures/semantic-b.json", {
      formatVersion: 1,
      producerId: "semantic-b",
      facts: authored.filter((f) => f.ref === "fact-b"),
    });
  }
  const captures = [
    ["native", "executable", "captures/native.txt"],
    ["semantic", "executable", "captures/semantic.txt"],
    ["toolchain", "toolchain", "captures/toolchain.txt"],
    ["config", "config", "captures/config.txt"],
    ["dependency", "dependency", "captures/dependency.txt"],
    ["artifact", "semanticArtifact", "captures/semantic.json"],
  ].map(([ref, kind, file]) => ({ ref, kind, file, hash: sha(fs.get(file)) }));
  if (twoProducers)
    captures.push(
      ...[
        ["semantic-b-executable", "executable", "captures/semantic-b.txt"],
        ["semantic-b-artifact", "semanticArtifact", "captures/semantic-b.json"],
      ].map(([ref, kind, file]) => ({
        ref,
        kind,
        file,
        hash: sha(fs.get(file)),
      })),
    );
  const revision = {
    id: "r1",
    sourceSetId: "main",
    documents: [
      { key: document, revisionId: "r1", sourceFile: "snapshots/src/main.js" },
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
    language: document.language,
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: sha(
      canonical(
        alternateTuples
          ? [
              { document, contentHash: sourceHash },
              {
                document: { ...document, path: "src/other.js" },
                contentHash: sha(alternateText("document")),
              },
            ]
          : [{ document, contentHash: sourceHash }],
      ),
    ),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const proof = {
    id: "proof",
    producerId: "semantic",
    document,
    revisionId: "r1",
    contentHash: sourceHash,
    evidenceKind:
      family === "declarationName" ? "declarationBinding" : "semanticReference",
    basis,
    freshness: "fresh",
  };
  const proofs = authored.map((fact) => {
    const evidenceKind =
      fact.kind === "typeRelationship"
        ? "typeRelationship"
        : fact.kind === "declarationBinding"
          ? "declarationBinding"
          : "semanticReference";
    return fact.ref === "fact-b"
      ? {
          ...proof,
          evidenceKind,
          id: "proof-b",
          producerId: "semantic-b",
          basis: {
            ...basis,
            producerId: "semantic-b",
            producerHash: sha("semantic-b-executable"),
            artifactHash: sha(fs.get("captures/semantic-b.json")),
          },
          freshness: "possiblyStale",
        }
      : {
          ...proof,
          evidenceKind,
          id: fact.provenanceRef ?? fact.record.provenanceId,
        };
  });
  if (comparisonSemanticVersion !== null)
    for (const proof of proofs)
      if (proof.producerId === "semantic") proof.freshness = "possiblyStale";
  const proofFacts = proofs.map((record, i) => ({
    kind: "provenance",
    ref: `proof-fact-${i}`,
    record,
  }));
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({
      kind,
      available: !(status === "unsupported" && kind === unsupportedFamily),
      diagnostic:
        status === "unsupported" && kind === unsupportedFamily
          ? "native family unavailable"
          : null,
    }),
  );
  mutateSupport?.(support);
  const roleOrder = [
    "definition",
    "read",
    "write",
    "call",
    "type",
    "import",
    "alias",
  ];
  const orderedRoles = (values) =>
    [...new Set(values)].sort(
      (a, b) => roleOrder.indexOf(a) - roleOrder.indexOf(b),
    );
  const rolesFor = (producerId) =>
    authored
      .filter((fact) =>
        producerId === "semantic-b"
          ? fact.ref === "fact-b"
          : fact.ref !== "fact-b",
      )
      .flatMap((fact) => {
        if (
          fact.kind === "reference" &&
          status === "exact" &&
          support.find((x) => x.kind === "reference").available
        )
          return fact.record.roles;
        if (
          fact.kind === "callBinding" &&
          status === "exact" &&
          support.find((x) => x.kind === fact.anchor.kind).available
        )
          return ["call"];
        return [];
      });
  const coverage = (producerId) => {
    const selected = producerId !== "semantic-b" || selectSecond;
    const roles = producerId === "native" ? [] : rolesFor(producerId);
    const observed = selected
      ? orderedRoles([
          ...(support.find((x) => x.kind === "reference").available
            ? ["read"]
            : []),
          ...roles,
        ])
      : [];
    const incomplete =
      status === "unsupported" && unsupportedFamily === "reference";
    return {
      producerId,
      language: document.language,
      sourceSetId: document.sourceSetId,
      documentPath: document.path,
      revisionId: "r1",
      requested: true,
      selected,
      state: !selected ? "omitted" : incomplete ? "partial" : "complete",
      supportedRoles: orderedRoles(["read", ...roles]),
      observedRoles: observed,
      diagnostic: !selected
        ? "producer not selected"
        : incomplete
          ? "reference unavailable"
          : null,
    };
  };
  put("snapshots/src/main.js.annotations.json", {
    formatVersion: 1,
    document,
    revisionId: "r1",
    scenarios,
    facts: [
      ...(twoProducers
        ? ["native", "semantic", "semantic-b"]
        : ["native", "semantic"]
      ).map((id) => ({
        kind: "coverage",
        ref: `coverage-${id}`,
        record: mutateCoverage?.(coverage(id)) ?? coverage(id),
      })),
      ...proofFacts,
      ...authored,
    ],
  });
  const selectedSemantic = { ...semanticProducer, positionEncoding: encoding };
  const unselectedSemantic = {
    ...semanticProducer,
    id: "semantic-b",
    executableHash: sha("semantic-b-executable"),
    positionEncoding: secondEncoding ?? encoding,
  };
  const producers = (
    twoProducers
      ? [nativeProducer, selectedSemantic, unselectedSemantic]
      : [nativeProducer, selectedSemantic]
  ).map((p) => ({ ...p, languages: [document.language] }));
  const fixture = {
    formatVersion: 1,
    profile,
    language: document.language,
    sourceSets: [
      {
        id: "main",
        rootId: "root",
        languages: [document.language],
        dependencies: [],
      },
    ],
    producers,
    revisions: [revision],
    comparison: {
      sourceSetId: "main",
      revisionId: "r1",
      producers: (includeSecondInComparison
        ? producers
        : producers.slice(0, 2)
      ).map((producer) =>
        comparisonSemanticVersion !== null && producer.id === "semantic"
          ? { ...producer, version: comparisonSemanticVersion }
          : producer,
      ),
    },
    coverageIntents: producers.map((producer) => ({
      producerId: producer.id,
      document,
      revisionId: "r1",
      requestedRoles: orderedRoles([
        ...requestedRoles,
        ...(producer.id === "native" ? [] : rolesFor(producer.id)),
      ]),
      measurementSupport: support,
    })),
    nativeArtifact: "captures/native.json",
    semanticArtifacts: twoProducers
      ? ["captures/semantic.json", "captures/semantic-b.json"]
      : ["captures/semantic.json"],
    annotationFiles: ["snapshots/src/main.js.annotations.json"],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  if (alternateTuples) {
    const alternatives = [
      {
        document: { ...document, path: "src/other.js" },
        revisionId: "r1",
        file: "snapshots/src/other.js",
        kind: "document",
      },
      {
        document,
        revisionId: "r2",
        file: "snapshots/r2/src/main.js",
        kind: "revision",
      },
      {
        document: { ...document, sourceSetId: "alternate" },
        revisionId: "r1",
        file: "snapshots/alternate/src/main.js",
        kind: "source set",
      },
    ];
    fixture.revisions[0].documents.push({
      key: alternatives[0].document,
      revisionId: "r1",
      sourceFile: alternatives[0].file,
    });
    fixture.revisions.push({
      ...revision,
      id: "r2",
      documents: [
        { key: document, revisionId: "r2", sourceFile: alternatives[1].file },
      ],
    });
    fixture.revisions.push({
      ...revision,
      sourceSetId: "alternate",
      documents: [
        {
          key: alternatives[2].document,
          revisionId: "r1",
          sourceFile: alternatives[2].file,
        },
      ],
    });
    fixture.sourceSets.push({
      id: "alternate",
      rootId: "alternate-root",
      languages: [document.language],
      dependencies: [],
    });
    for (const alt of alternatives) {
      put(alt.file, alternateText(alt.kind));
      const rows = producers.map((producer) => ({
        kind: "coverage",
        ref: `coverage-${producer.id}-${alt.revisionId}-${alt.document.path}`,
        record: {
          ...coverage(producer.id),
          sourceSetId: alt.document.sourceSetId,
          documentPath: alt.document.path,
          revisionId: alt.revisionId,
        },
      }));
      const annotation = `${alt.file}.annotations.json`;
      put(annotation, {
        formatVersion: 1,
        document: alt.document,
        revisionId: alt.revisionId,
        scenarios: [],
        facts: rows,
      });
      fixture.annotationFiles.push(annotation);
      for (const producer of producers)
        fixture.coverageIntents.push({
          producerId: producer.id,
          document: alt.document,
          revisionId: alt.revisionId,
          requestedRoles: ["read"],
          measurementSupport: structuredClone(support),
        });
    }
    // The captured semantic proof's manifest includes both documents of r1/main.
  }
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put(
    "expected/dispositions.json",
    dispositions ?? {
      formatVersion: 1,
      assertions: [],
      callableValueNegatives: [],
    },
  );
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  for (const [path, value] of fs) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await writeFile(join(root, path), value);
  }
  const loaded = await loadFixture(root),
    records = absent();
  records.comparison = fixture.comparison;
  records.producers = ordered(structuredClone(fixture.producers));
  records.sourceSets = ordered(structuredClone(fixture.sourceSets));
  records.revisions = ordered(
    fixture.revisions.map((rev) => ({
      ...rev,
      documents: rev.documents.map((item) => ({
        key: item.key,
        revisionId: item.revisionId,
        contentHash: sha(fs.get(item.sourceFile)),
        byteLength: Buffer.byteLength(fs.get(item.sourceFile)),
      })),
    })),
  );
  records.provenance = proofs;
  records.coverage = ordered(
    loaded.annotations.flatMap((annotation) =>
      annotation.facts
        .filter((f) => f.kind === "coverage")
        .map((f) => f.record),
    ),
  );
  const asDeclaration = (name, begin, end, nameStart) => ({
    syntaxId: syntax(name),
    role: "production",
    document,
    revisionId: "r1",
    kind: "function",
    name,
    lookupKey: name,
    ancestors: [],
    key: { kind: "function", name, signature: null, ordinal: 0 },
    range: range(begin, end),
    nameRange: range(nameStart, nameStart + name.length),
    header: header(name),
    provenanceId: `native:r1:${syntax(name)}`,
  });
  records.declarations = [
    asDeclaration("main", 0 + offset, 39 + offset, 9 + offset),
    asDeclaration("target", 40 + offset, 60 + offset, 49 + offset),
  ].sort((a, b) =>
    Buffer.compare(Buffer.from(a.syntaxId), Buffer.from(b.syntaxId)),
  );
  records.calls = [
    {
      id: callId,
      ownerSyntaxId: mainId,
      ordinal: 0,
      document,
      revisionId: "r1",
      range: range(18 + offset, 26 + offset),
      calleeRange: range(18 + offset, 24 + offset),
      spelling: "target",
      regionIds: control ? [occurrence(mainId, "control", 0, sourceText)] : [],
      provenanceId: `native:r1:${callId}`,
    },
  ];
  if (control) {
    const id = occurrence(mainId, "control", 0, sourceText);
    records.controlRegions = [
      {
        id,
        ownerSyntaxId: mainId,
        ordinal: 0,
        document,
        revisionId: "r1",
        kind: "if",
        range: range(18 + offset, 26 + offset),
        parentId: null,
        arm: null,
        provenanceId: `native:r1:${id}`,
      },
    ];
  }
  if (normalized) {
    const normalizedRecords = normalizeFixture(loaded).records;
    for (const name of ["producers", "sourceSets", "revisions"])
      normalizedRecords[name] = ordered(normalizedRecords[name]);
    return { loaded, records: normalizedRecords, authored };
  }
  const coverageResult = checkCoverage(loaded, records);
  const measurement = checkMeasurement(loaded, records);
  const expectedJoin = authored.map((fact) => {
    const bytes = [fact.anchor.range.start, fact.anchor.range.end].map(
      (n) => n - positionOffset + offset,
    );
    const id =
      fact.anchor.kind === "reference" && bytes[0] === 28 + offset
        ? callbackId
        : declarationSite &&
            fact.anchor.kind === "reference" &&
            bytes[0] === 49 + offset
          ? occurrence(targetId, "reference", 0, sourceText)
          : {
              declarationName: targetId,
              callee: callId,
              invocation: callId,
              reference: refId,
            }[fact.anchor.kind];
    return {
      anchor: {
        document,
        revisionId: "r1",
        contentHash: sourceHash,
        range: range(...bytes),
        kind: fact.anchor.kind,
      },
      status,
      candidateIds: status === "exact" ? [id] : [],
      diagnostic:
        status === "exact"
          ? null
          : status === "unsupported"
            ? "native family unavailable"
            : "unmatched",
    };
  });
  for (const [i, fact] of authored.entries())
    if (fact.kind === "reference") {
      if (status === "exact") {
        const callback = fact.anchor.range.start === 28 + positionOffset,
          atDeclaration =
            declarationSite && fact.anchor.range.start === 49 + positionOffset;
        const measuredRange = expectedJoin[i].anchor.range;
        const external = {
          kind: "external",
          symbol: {
            scheme: "scip",
            symbol: "pkg external",
            scope: "global",
            document: null,
          },
        };
        const resolution = fact.record.resolution;
        records.references.push({
          id: atDeclaration
            ? occurrence(targetId, "reference", 0, sourceText)
            : callback
              ? callbackId
              : refId,
          ownerSyntaxId: atDeclaration ? targetId : mainId,
          ordinal: callback ? 1 : 0,
          document,
          revisionId: "r1",
          range: measuredRange,
          spelling: callback ? "callback" : "target",
          lookupKey: callback ? "callback" : "target",
          site: fact.record.site,
          roles: fact.record.roles,
          resolution,
          declaredTarget:
            resolution === "resolved"
              ? target
              : resolution === "external"
                ? external
                : null,
          candidates:
            resolution === "ambiguous" ? ordered([target, external]) : [],
          provenanceId: fact.record.provenanceId,
        });
      } else
        records.referenceJoinDiagnostics.push({
          factRef: fact.ref,
          provenanceId: fact.record.provenanceId,
          join: expectedJoin[i],
        });
    }
  records.references = ordered([
    ...new Map(records.references.map((row) => [canonical(row), row])).values(),
  ]);
  records.referenceJoinDiagnostics.sort((a, b) =>
    Buffer.compare(Buffer.from(a.factRef), Buffer.from(b.factRef)),
  );
  return {
    loaded,
    records,
    coverageResult,
    measurement,
    authored,
    expectedJoin,
  };
}

test("source-backed counts: exact read and measured call", async (t) => {
  const s = await specimen(t);
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(actual.measuredCalls, 1);
  assert.equal(actual.references, 1);
  assert.deepEqual(actual.observedRoles, ["read", "call"]);
});

const result = (loaded, records) => checkCounts(loaded, records);
const dispositionKind = {
  resolved: "resolved",
  external: "provenExternal",
  ambiguous: "ambiguous",
  unresolved: "unresolved",
};
for (const [resolution, disposition] of Object.entries(dispositionKind))
  test(`captured ${disposition} disposition`, async (t) => {
    const specimenResult = await specimen(t, {
      mutateFact: (fact) => {
        fact.record.resolution = resolution;
        fact.record.declaredTarget =
          resolution === "resolved"
            ? declaredTarget
            : resolution === "external"
              ? {
                  kind: "external",
                  symbol: {
                    scheme: "scip",
                    symbol: "pkg external",
                    scope: "global",
                    document: null,
                  },
                }
              : null;
        fact.record.candidates =
          resolution === "ambiguous"
            ? [
                declaredTarget,
                {
                  kind: "external",
                  symbol: {
                    scheme: "scip",
                    symbol: "pkg external",
                    scope: "global",
                    document: null,
                  },
                },
              ]
            : [];
      },
    });
    specimenResult.loaded.dispositions.assertions.push({
      kind: "resolution",
      factRef: "fact",
      disposition,
    });
    assert.equal(
      result(specimenResult.loaded, specimenResult.records).outcomes.find(
        (x) => x.disposition === disposition,
      ).count,
      1,
    );
  });
test("unsupported is checked against native source capability", async (t) => {
  const s = await specimen(t, {
    status: "unsupported",
    family: "reference",
    mutateFact: (fact) => {
      fact.anchor.range = span(0, 8);
    },
  });
  s.loaded.dispositions.assertions.push({
    kind: "join",
    factRef: "fact",
    disposition: "unsupported",
  });
  assert.equal(
    result(s.loaded, s.records).outcomes.find(
      (x) => x.disposition === "unsupported",
    ).count,
    1,
  );
});
test("callable reference outside callee counts as negative, not a second call", async (t) => {
  const s = await specimen(t, { callbackSite: true });
  const anchor = s.authored[0].anchor;
  s.loaded.annotations[0].scenarios.push({
    id: "callback",
    category: "callableValues",
    anchors: [anchor],
    factRefs: ["fact"],
  });
  s.loaded.dispositions.callableValueNegatives.push({
    scenarioId: "callback",
    referenceRef: "fact",
    ownerRef: "main",
    range: anchor.range,
  });
  const value = result(s.loaded, s.records);
  assert.equal(value.references, 1);
  assert.equal(value.callableValueNegatives, 1);
  assert.equal(value.measuredCalls, 1);
});
test("unsupported source capability rejects an authored label", async (t) => {
  const s = await specimen(t);
  const rows = registerControls([
    {
      id: "COUNT.disposition.unsupported-label",
      baseline: () => s.loaded.dispositions,
      mutate: (rows) => {
        rows.assertions.push({
          kind: "join",
          factRef: "fact",
          disposition: "unsupported",
        });
        return rows;
      },
      check: (rows) => checkCounts(s.loaded, s.records, rows),
      expectedAssertion: "COUNT.DISPOSITION",
      expectedCode: "invalidRecord",
      expectedField: "assertions",
    },
  ]);
  for (const row of rows) await runControl(row);
});
test("corpus profile enforces all floors, not only totals", async (t) => {
  const s = await specimen(t);
  const controls = registerControls([
    {
      id: "COUNT.profile.example-to-corpus",
      baseline: () => s.loaded.fixture,
      mutate: (fixture) => {
        fixture.profile = "corpus";
        return fixture;
      },
      check: (fixture) => checkCounts({ ...s.loaded, fixture }, s.records),
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("draft language corpus skips only count floors; example and draft stay in their own directories", async (t) => {
  const s = await specimen(t);
  const asLanguage = (fixture) => ({
    ...s.loaded,
    root: s.loaded.root.replace(/example$/, "javascript"),
    fixture,
  });
  const draft = checkCounts(
    asLanguage({ ...s.loaded.fixture, profile: "draft" }),
    s.records,
  );
  assert.equal(draft.profile, "draft");
  assert.equal(draft.floorsEnforced, false);
  const controls = registerControls([
    {
      id: "COUNT.profile.final-corpus-enforces-floors",
      baseline: () => ({ ...s.loaded.fixture, profile: "draft" }),
      mutate: (fixture) => ({ ...fixture, profile: "corpus" }),
      check: (fixture) => checkCounts(asLanguage(fixture), s.records),
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    },
    {
      id: "COUNT.profile.draft-not-in-example",
      baseline: () => s.loaded.fixture,
      mutate: (fixture) => ({ ...fixture, profile: "draft" }),
      check: (fixture) => checkCounts({ ...s.loaded, fixture }, s.records),
      expectedAssertion: "COUNT.PROFILE",
      expectedCode: "invalidRecord",
      expectedField: "profile",
    },
    {
      id: "COUNT.profile.draft-still-checks-invariants",
      baseline: () => s.records,
      mutate: (records) => {
        records.references[0].id = `occ:v2:${"b".repeat(32)}`;
        return records;
      },
      check: (records) =>
        checkCounts(
          asLanguage({ ...s.loaded.fixture, profile: "draft" }),
          records,
        ),
      expectedAssertion: "REFERENCE.SOURCE",
      expectedCode: "invalidRecord",
      expectedField: "references",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("exact mutation controls on source-backed native membership and captured dispositions", async (t) => {
  const sample = await specimen(t);
  const disposition = await specimen(t);
  disposition.loaded.dispositions.assertions.push({
    kind: "resolution",
    factRef: "fact",
    disposition: "resolved",
  });
  const controls = registerControls([
    {
      id: "COUNT.calls.forged-owner-span-provenance",
      baseline: () => sample.records,
      mutate: (records) => {
        const forged = `occ:v2:${"a".repeat(32)}`;
        records.calls[0] = {
          ...records.calls[0],
          id: forged,
          ownerSyntaxId: targetId,
          range: range(49, 55),
          calleeRange: range(49, 55),
          provenanceId: `native:r1:${forged}`,
        };
        return records;
      },
      check: (records) => result(sample.loaded, records),
      expectedAssertion: "COUNT.CALLS",
      expectedCode: "invalidRecord",
      expectedField: "measuredCalls",
    },
    {
      id: "COUNT.disposition.source-proof",
      baseline: () => disposition.loaded.dispositions,
      mutate: (rows) => {
        rows.assertions[0].disposition = "unresolved";
        return rows;
      },
      check: (rows) =>
        checkCounts(disposition.loaded, disposition.records, rows),
      expectedAssertion: "COUNT.DISPOSITION",
      expectedCode: "invalidRecord",
      expectedField: "assertions",
    },
    {
      id: "COUNT.reference.native-id",
      baseline: () => sample.records,
      mutate: (records) => {
        records.references[0].id = `occ:v2:${"b".repeat(32)}`;
        return records;
      },
      check: (records) => result(sample.loaded, records),
      expectedAssertion: "REFERENCE.SOURCE",
      expectedCode: "invalidRecord",
      expectedField: "references",
    },
  ]);
  for (const row of controls) await runControl(row);
});

test("normalizer fixture call candidate", async (t) => {
  const s = await specimen(t, { family: "callee" });
  const normal = normalizeFixture(s.loaded);
  for (const key of ["producers", "sourceSets", "revisions"])
    normal.records[key] = ordered(normal.records[key]);
  const actual = checkCounts(s.loaded, normal.records);
  assert.equal(actual.measuredCalls, 1);
});

test("category-specific source witnesses and unique scenario anchors", async (t) => {
  const sample = await specimen(t, { callbackSite: true });
  const annotation = sample.loaded.annotations[0];
  const one = {
    id: "callback",
    category: "callableValues",
    anchors: [sample.authored[0].anchor],
    factRefs: ["fact"],
  };
  annotation.scenarios.push(one);
  assert.equal(
    checkCounts(sample.loaded, sample.records).scenariosByCategory.find(
      (x) => x.category === "callableValues",
    ).count,
    1,
  );
  const rows = registerControls([
    {
      id: "COUNT.scenario.category-relabel",
      baseline: () => sample.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].category = "recursion";
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...sample.loaded, annotations }, sample.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "category",
    },
    {
      id: "COUNT.scenario.anchor-copy",
      baseline: () => sample.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios.push({
          ...annotations[0].scenarios[0],
          id: "copied",
        });
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...sample.loaded, annotations }, sample.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "anchors",
    },
    {
      id: "COUNT.scenario.unconnected-fact",
      baseline: () => sample.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].factRefs = ["coverage-native"];
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...sample.loaded, annotations }, sample.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "factRefs",
    },
  ]);
  for (const row of rows) await runControl(row);
});
test("captured import usage supports only importsAliases, not callableValues", async (t) => {
  const s = await specimen(t, {
    mutateFact: (fact) => {
      fact.record.roles = ["import"];
    },
  });
  const anchor = s.authored[0].anchor;
  s.loaded.annotations[0].scenarios.push({
    id: "import",
    category: "importsAliases",
    anchors: [anchor],
    factRefs: ["fact"],
  });
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(
    actual.scenariosByCategory.find((x) => x.category === "importsAliases")
      .count,
    1,
  );
  assert.deepEqual(actual.observedRoles, ["import"]);
});

test("normalized source-backed recursive call has one call and one anchored recursion scenario", async (t) => {
  const s = await specimen(t, {
    family: "callee",
    normalized: true,
    mutateFact: (fact) => {
      fact.record.declaredTarget = {
        kind: "internal",
        declarationRef: "main",
        revisionId: "r1",
      };
    },
  });
  s.loaded.annotations[0].scenarios.push({
    id: "self",
    category: "recursion",
    anchors: [s.authored[0].anchor],
    factRefs: ["fact"],
  });
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(
    actual.scenariosByCategory.find((x) => x.category === "recursion").count,
    1,
  );
  assert.equal(actual.measuredCalls, 1);
});

test("nested declaration-name anchors use the measured parent, not the focused declaration", async (t) => {
  const s = await specimen(t, {
    normalized: true,
    mutateNative: (native) => {
      native.declarations[0].range.end = 60;
      native.declarations[1].parentRef = "main";
    },
  });
  const anchor = {
    ...s.authored[0].anchor,
    kind: "declarationName",
    range: span(49, 55),
    ownerRef: "main",
  };
  s.loaded.annotations[0].scenarios.push({
    id: "nested",
    category: "compatibilityControl",
    anchors: [s.authored[0].anchor, anchor],
    factRefs: ["fact"],
  });
  assert.equal(checkCounts(s.loaded, s.records).scenariosTotal, 1);
  const controls = registerControls([
    {
      id: "COUNT.scenario.nested-owner",
      baseline: () => s.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].anchors[1].ownerRef = "target";
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...s.loaded, annotations }, s.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "anchors",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("member-name reference inside a larger measured callee cannot be a callable-value negative", async (t) => {
  const s = await specimen(t, {
    normalized: true,
    mutateNative: (native) => {
      native.calls[0].range.start = 17;
      native.calls[0].calleeRange.start = 17;
      native.calls[0].spelling = " target";
      native.calls[0].witnesses[0].witness = {
        range: span(17, 24),
        text: " target",
      };
    },
    mutateFact: (fact) => {
      fact.record.roles = ["read"];
    },
  });
  s.loaded.annotations[0].scenarios.push({
    id: "callee-member",
    category: "callableValues",
    anchors: [s.authored[0].anchor],
    factRefs: ["fact"],
  });
  assert.equal(checkCounts(s.loaded, s.records).scenariosTotal, 1);
  const controls = registerControls([
    {
      id: "COUNT.negative.compound-callee",
      baseline: () => s.loaded.dispositions,
      mutate: (rows) => {
        rows.callableValueNegatives.push({
          scenarioId: "callee-member",
          referenceRef: "fact",
          ownerRef: "main",
          range: s.authored[0].anchor.range,
        });
        return rows;
      },
      check: (rows) => checkCounts(s.loaded, s.records, rows),
      expectedAssertion: "COUNT.NEGATIVE",
      expectedCode: "invalidRecord",
      expectedField: "callableValueNegatives",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("UTF-16 callable negative converts to UTF-8 source before comparison", async (t) => {
  const s = await specimen(t, {
    callbackSite: true,
    encoding: "utf16",
    emojiPrefix: true,
  });
  const anchor = s.authored[0].anchor;
  s.loaded.annotations[0].scenarios.push({
    id: "unicode-read",
    category: "callableValues",
    anchors: [anchor],
    factRefs: ["fact"],
  });
  s.loaded.dispositions.callableValueNegatives.push({
    scenarioId: "unicode-read",
    referenceRef: "fact",
    ownerRef: "main",
    range: anchor.range,
  });
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(actual.callableValueNegatives, 1);
  assert.equal(actual.references, 1);
});

test("two independently captured producers cannot inflate one measured reference", async (t) => {
  const s = await specimen(t, { twoProducers: true, selectSecond: true });
  s.loaded.fixture.comparison.producers.push(
    s.loaded.fixture.producers.find((p) => p.id === "semantic-b"),
  );
  s.records.comparison = structuredClone(s.loaded.fixture.comparison);
  s.records.provenance.find((p) => p.id === "proof-b").freshness = "fresh";
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(actual.references, 1);
});

test("same-name overload category requires two independently measured sibling names", async (t) => {
  const renamed = source.replace("function target()", "function main  ()");
  const s = await specimen(t, {
    family: "declarationName",
    normalized: true,
    sourceOverride: renamed,
    mutateNative: (native) => {
      const row = native.declarations[1];
      row.name = "main";
      row.nameRange.end = 53;
      row.header.name = "main";
      for (const witness of row.witnesses) {
        witness.witness.text = "main";
        witness.witness.range.end = 53;
      }
    },
    mutateFact: (fact) => {
      fact.anchor.range.end = 53;
    },
  });
  const other = {
    ...s.authored[0].anchor,
    range: span(9, 13),
    ownerRef: "main",
  };
  s.loaded.annotations[0].scenarios.push({
    id: "overload",
    category: "sameNameOverload",
    anchors: [other, s.authored[0].anchor],
    factRefs: ["fact"],
  });
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory.find(
      (x) => x.category === "sameNameOverload",
    ).count,
    1,
  );
});

test("unicode coordinates require source bytes; ASCII anchors cannot be relabelled", async (t) => {
  const s = await specimen(t, { encoding: "utf16", emojiPrefix: true });
  s.loaded.annotations[0].scenarios.push({
    id: "unicode",
    category: "unicodeCoordinates",
    anchors: [s.authored[0].anchor],
    factRefs: ["fact"],
  });
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory.find(
      (x) => x.category === "unicodeCoordinates",
    ).count,
    1,
  );
  const ascii = await specimen(t);
  ascii.loaded.annotations[0].scenarios.push({
    id: "ascii",
    category: "compatibilityControl",
    anchors: [ascii.authored[0].anchor],
    factRefs: ["fact"],
  });
  const controls = registerControls([
    {
      id: "COUNT.scenario.ascii-relabel",
      baseline: () => ascii.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].category = "unicodeCoordinates";
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...ascii.loaded, annotations }, ascii.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "category",
    },
  ]);
  for (const control of controls) await runControl(control);
});
test("coverageFreshness requires an authenticated partial tuple, not a relabelled scenario", async (t) => {
  const s = await specimen(t);
  const intent = s.loaded.fixture.coverageIntents.find(
    (x) => x.producerId === "semantic",
  );
  intent.requestedRoles = ["read", "write", "call"];
  const row = s.loaded.annotations[0].facts.find(
    (x) => x.kind === "coverage" && x.record.producerId === "semantic",
  ).record;
  row.state = "partial";
  row.diagnostic = "write not observed";
  const stored = s.records.coverage.find((x) => x.producerId === "semantic");
  stored.state = "partial";
  stored.diagnostic = "write not observed";
  s.loaded.annotations[0].scenarios.push({
    id: "partial",
    category: "coverageFreshness",
    anchors: [s.authored[0].anchor],
    factRefs: ["fact"],
  });
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory.find(
      (x) => x.category === "coverageFreshness",
    ).count,
    1,
  );
});

test("relationshipsDispatch requires a captured dispatch fact, not a reference name", async (t) => {
  const s = await specimen(t, { family: "callee", normalized: true });
  s.loaded.annotations[0].scenarios.push({
    id: "dispatch",
    category: "relationshipsDispatch",
    anchors: [s.authored[0].anchor],
    factRefs: ["fact"],
  });
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory.find(
      (x) => x.category === "relationshipsDispatch",
    ).count,
    1,
  );
  const ref = await specimen(t);
  ref.loaded.annotations[0].scenarios.push({
    id: "plain-read",
    category: "compatibilityControl",
    anchors: [ref.authored[0].anchor],
    factRefs: ["fact"],
  });
  const controls = registerControls([
    {
      id: "COUNT.scenario.dispatch-relabel",
      baseline: () => ref.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].category = "relationshipsDispatch";
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...ref.loaded, annotations }, ref.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "category",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("captured directed relationship counts once and rejects wrong direction or producer copy", async (t) => {
  const text = `${source}class Child extends Base {}\nclass Base {}\n`;
  const child = text.indexOf("class Child"),
    base = text.indexOf("class Base");
  const relationship = {
    kind: "typeRelationship",
    ref: "extends-child",
    source: { kind: "internal", declarationRef: "child", revisionId: "r1" },
    target: { kind: "internal", declarationRef: "base", revisionId: "r1" },
    relationshipKind: "extends",
    provenanceRef: "proof-extends-child",
  };
  const s = await specimen(t, {
    normalized: true,
    sourceOverride: text,
    facts: [makeFact("reference", "exact"), relationship],
    mutateNative: (rows) => {
      const type = (ref, name, begin, end, bases = []) => ({
        ref,
        nativeId: null,
    role: "production",
        document,
        revisionId: "r1",
        parentRef: null,
        kind: "type",
        name,
        range: span(begin, end),
        nameRange: span(begin + 6, begin + 6 + name.length),
        header: {
          kind: "type",
          name,
          modifiers: [],
          typeParameters: [],
          parameters: [],
          resultType: null,
          bases,
        },
        signature: null,
        witnesses: [
          witness("name", begin + 6, begin + 6 + name.length, name),
          witness("header.name", begin + 6, begin + 6 + name.length, name),
          ...bases.map((value, i) =>
            witness(
              `header.bases[${i}]`,
              text.indexOf(value, begin + 6),
              text.indexOf(value, begin + 6) + value.length,
              value,
            ),
          ),
        ],
      });
      rows.declarations.push(
        type("child", "Child", child, text.indexOf("\n", child), ["Base"]),
        type("base", "Base", base, text.indexOf("\n", base)),
      );
    },
  });
  const proof = s.loaded.annotations[0].facts.find(
    (f) => f.ref === "extends-child",
  );
  const row = s.loaded.native.declarations.find((d) => d.ref === "child");
  const anchor = {
    document,
    revisionId: "r1",
    contentHash: sha(text),
    kind: "declarationName",
    range: row.nameRange,
    ownerRef: "child",
  };
  s.loaded.annotations[0].scenarios.push({
    id: "directed",
    category: "relationshipsDispatch",
    anchors: [anchor],
    factRefs: ["extends-child"],
  });
  assert.equal(checkCounts(s.loaded, s.records).typeRelationships, 1);
  const controls = registerControls([
    {
      id: "COUNT.relationship.reverse-direction",
      baseline: () => s.records,
      mutate: (records) => {
        const edge = records.typeRelationships[0];
        [edge.source, edge.target] = [edge.target, edge.source];
        return records;
      },
      check: (records) => checkCounts(s.loaded, records),
      expectedAssertion: "RELATIONSHIP.SOURCE",
      expectedCode: "invalidRecord",
      expectedField: "source",
    },
    {
      id: "COUNT.relationship.producer-copy",
      baseline: () => s.records,
      mutate: (records) => {
        records.typeRelationships[0].provenanceId = "proof";
        return records;
      },
      check: (records) => checkCounts(s.loaded, records),
      expectedAssertion: "RELATIONSHIP.PROOF",
      expectedCode: "invalidRecord",
      expectedField: "provenanceId",
    },
  ]);
  for (const control of controls) await runControl(control);
});

for (const role of ["write", "type"])
  test(`source-backed ${role} reference role`, async (t) => {
    const s = await specimen(t, {
      mutateFact: (fact) => {
        fact.record.roles = [role];
      },
    });
    assert.deepEqual(checkCounts(s.loaded, s.records).observedRoles, [role]);
  });
test("measured declaration site supports definition and alias roles once per reference", async (t) => {
  const s = await specimen(t, {
    declarationSite: true,
    mutateFact: (fact) => {
      fact.record.site = "declaration";
      fact.record.roles = ["definition", "alias"];
    },
  });
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(actual.references, 1);
  assert.deepEqual(actual.observedRoles, ["definition", "alias"]);
});

// Independently authored Decision 0017 Amendment 1 floors, per language.
const amendedFloors = {
  javascript: { ambiguous: 20, typeRelationships: 20, exempt: [] },
  java: { ambiguous: 0, typeRelationships: 20, exempt: ["alias"] },
  python: { ambiguous: 0, typeRelationships: 20, exempt: ["alias"] },
  rust: { ambiguous: 0, typeRelationships: 0, exempt: [] },
};
const allRoles = [
  "definition",
  "read",
  "write",
  "call",
  "type",
  "import",
  "alias",
];
const countAtFloors = (language) => {
  const floors = amendedFloors[language];
  return {
    formatVersion: 1,
    language,
    profile: "corpus",
    floorsEnforced: true,
    scenariosTotal: 32,
    scenariosByCategory: [
      "sameNameOverload",
      "importsAliases",
      "callableValues",
      "recursion",
      "relationshipsDispatch",
      "unicodeCoordinates",
      "coverageFreshness",
      "compatibilityControl",
    ].map((category) => ({ category, count: 4 })),
    measuredCalls: 120,
    references: 40,
    callableValueNegatives: 20,
    typeRelationships: floors.typeRelationships,
    outcomes: [
      ["resolved", 20],
      ["provenExternal", 20],
      ["ambiguous", floors.ambiguous],
      ["unresolved", 20],
      ["unsupported", 1],
    ].map(([disposition, count]) => ({ disposition, count })),
    observedRoles: allRoles.filter((role) => !floors.exempt.includes(role)),
  };
};

test("each independent corpus minimum and each nonzero category and outcome floor, per language", async () => {
  for (const language of Object.keys(amendedFloors)) {
    const full = countAtFloors(language);
    assert.equal(assertCorpusFloors(full), full);
    const mutations = [
      ...[
        "scenariosTotal",
        "measuredCalls",
        "references",
        "callableValueNegatives",
        "typeRelationships",
      ]
        .filter((field) => full[field] > 0)
        .map((field) => (row) => {
          row[field]--;
        }),
      ...full.scenariosByCategory.map((_, i) => (row) => {
        row.scenariosByCategory[i].count--;
      }),
      ...full.outcomes
        .map((x, i) => [x, i])
        .filter(([x]) => x.count > 0)
        .map(([, i]) => (row) => {
          row.outcomes[i].count--;
        }),
      ...full.observedRoles.map((role) => (row) => {
        row.observedRoles = row.observedRoles.filter((value) => value !== role);
      }),
    ];
    const controls = registerControls(
      mutations.map((mutate, i) => ({
        id: `COUNT.floor.boundary.${language}.${i}`,
        baseline: () => full,
        mutate: (row) => {
          mutate(row);
          return row;
        },
        check: assertCorpusFloors,
        expectedAssertion: "COUNT.FLOOR",
        expectedCode: "invalidRecord",
        expectedField: "counts",
      })),
    );
    for (const row of controls) await runControl(row);
  }
});

test("ambiguous floor is 0 for Python, Java and Rust and stays 20 for JavaScript", async () => {
  for (const language of ["python", "java", "rust"]) {
    const count = countAtFloors(language);
    assert.equal(
      count.outcomes.find((x) => x.disposition === "ambiguous").count,
      0,
    );
    assert.equal(assertCorpusFloors(count), count);
  }
  const controls = registerControls([
    {
      id: "COUNT.floor.javascript-ambiguous-19",
      baseline: () => countAtFloors("javascript"),
      mutate: (row) => {
        row.outcomes.find((x) => x.disposition === "ambiguous").count = 19;
        return row;
      },
      check: assertCorpusFloors,
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    },
  ]);
  for (const row of controls) await runControl(row);
});

test("Rust needs no type relationships; Java, JavaScript and Python still need 20", async () => {
  const rust = countAtFloors("rust");
  assert.equal(rust.typeRelationships, 0);
  assert.equal(assertCorpusFloors(rust), rust);
  const controls = registerControls(
    ["java", "javascript", "python"].map((language) => ({
      id: `COUNT.floor.${language}-relationships-19`,
      baseline: () => countAtFloors(language),
      mutate: (row) => {
        row.typeRelationships = 19;
        return row;
      },
      check: assertCorpusFloors,
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    })),
  );
  for (const row of controls) await runControl(row);
});

test("alias role is exempt for Java and Python and required for JavaScript and Rust", async () => {
  for (const language of ["java", "python"]) {
    const count = countAtFloors(language);
    assert.equal(count.observedRoles.includes("alias"), false);
    assert.equal(assertCorpusFloors(count), count);
  }
  const controls = registerControls(
    ["javascript", "rust"].map((language) => ({
      id: `COUNT.floor.${language}-alias-required`,
      baseline: () => countAtFloors(language),
      mutate: (row) => {
        row.observedRoles = row.observedRoles.filter((x) => x !== "alias");
        return row;
      },
      check: assertCorpusFloors,
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    })),
  );
  for (const row of controls) await runControl(row);
});

test("alias-name reference resolving to the original declaration counts toward alias", async (t) => {
  // The producer reports the alias name as a reference to the original
  // declaration; the measured alias name is a declaration site, and its target
  // is a different, original declaration.
  const s = await specimen(t, {
    declarationSite: true,
    mutateFact: (fact) => {
      fact.record.site = "declaration";
      fact.record.roles = ["definition", "alias"];
      fact.record.declaredTarget = {
        kind: "internal",
        declarationRef: "main",
        revisionId: "r1",
      };
    },
  });
  // Independently authored normalized row: the alias site targets `main`.
  s.records.references[0].declaredTarget = {
    kind: "internal",
    syntaxId: mainId,
    document,
    revisionId: "r1",
  };
  const actual = checkCounts(s.loaded, s.records);
  assert.equal(actual.references, 1);
  assert.deepEqual(actual.observedRoles, ["definition", "alias"]);
});

test("captured proof cannot be relabelled to an unrelated fact", async (t) => {
  const sample = await specimen(t);
  const rows = registerControls([
    {
      id: "COUNT.proof.capture-link",
      baseline: () => sample.loaded.semanticProofs,
      mutate: (proofs) => {
        proofs.get("proof").factRef = "unrelated";
        return proofs;
      },
      check: (proofs) =>
        checkCounts(
          { ...sample.loaded, semanticProofs: proofs },
          sample.records,
        ),
      expectedAssertion: "COUNT.PROOF",
      expectedCode: "invalidRecord",
      expectedField: "provenanceId",
    },
  ]);
  for (const row of rows) await runControl(row);
});

test("two captured semantic producers cannot inflate one unsupported source span", async (t) => {
  const s = await specimen(t, {
    status: "unsupported",
    family: "reference",
    twoProducers: true,
    selectSecond: true,
    mutateFact: (fact) => {
      fact.anchor.range = span(0, 8);
    },
  });
  s.loaded.fixture.comparison.producers.push(
    s.loaded.fixture.producers.find((p) => p.id === "semantic-b"),
  );
  s.records.comparison = structuredClone(s.loaded.fixture.comparison);
  s.records.provenance.find((p) => p.id === "proof-b").freshness = "fresh";
  s.loaded.dispositions.assertions.push(
    { kind: "join", factRef: "fact", disposition: "unsupported" },
    { kind: "join", factRef: "fact-b", disposition: "unsupported" },
  );
  assert.equal(
    checkCounts(s.loaded, s.records).outcomes.find(
      (x) => x.disposition === "unsupported",
    ).count,
    1,
  );
});

test("unsupported copies in UTF-16 and scalar encodings share one source-byte span", async (t) => {
  const s = await specimen(t, {
    status: "unsupported",
    family: "reference",
    encoding: "utf16",
    emojiPrefix: true,
    twoProducers: true,
    selectSecond: true,
    normalized: true,
    secondEncoding: "unicodeScalar",
    mutateFact: (fact) => {
      fact.anchor.range = span(3, 11, "utf16");
    },
    mutateSecond: (fact) => {
      fact.anchor.range = span(2, 10, "unicodeScalar");
    },
  });
  s.loaded.fixture.comparison.producers.push(
    s.loaded.fixture.producers.find((p) => p.id === "semantic-b"),
  );
  s.records.comparison = structuredClone(s.loaded.fixture.comparison);
  s.records.provenance.find((p) => p.id === "proof-b").freshness = "fresh";
  s.loaded.dispositions.assertions.push(
    { kind: "join", factRef: "fact", disposition: "unsupported" },
    { kind: "join", factRef: "fact-b", disposition: "unsupported" },
  );
  assert.equal(
    checkCounts(s.loaded, s.records).outcomes.find(
      (x) => x.disposition === "unsupported",
    ).count,
    1,
  );
});

// The floor specimen is a captured source/candidate inventory, not an authored CountsV1.
async function generatedCorpus(
  t,
  {
    duplicateProducer = false,
    omitRole = null,
    noOverloadSignatures = false,
    // Drop the 20 invocation-family facts and their unsupported assertions.
    withoutUnsupportedFacts = false,
    // Declare every native family available, so no document can qualify.
    allFamiliesAvailable = false,
  } = {},
) {
  const doc = {
    sourceSetId: "main",
    language: "java",
    path: "src/Corpus.java",
  };
  let text = "// é\n",
    decls = [],
    calls = [],
    refs = [],
    facts = [],
    scenarios = [];
  const add = (part) => {
    const start = Buffer.byteLength(text);
    text += part;
    return start;
  };
  const anchor = (kind, start, end, ownerRef) => ({
    document: doc,
    revisionId: "r1",
    contentHash: "0".repeat(64),
    kind,
    range: span(start, end),
    ownerRef,
  });
  const simple = (
    ref,
    kind,
    name,
    begin,
    end,
    nameAt,
    parentRef = null,
    params = [],
    bases = [],
  ) => {
    const header = {
      kind,
      name,
      modifiers: [],
      typeParameters: [],
      parameters: params.map(([type, value]) => ({
        type,
        name: value,
        variadic: false,
      })),
      resultType: null,
      bases,
    };
    const witnesses = [
      witness("name", nameAt, nameAt + name.length, name),
      witness("header.name", nameAt, nameAt + name.length, name),
    ];
    for (const [i, [type, value]] of params.entries()) {
      const at = Buffer.from(text).indexOf(type, begin);
      witnesses.push(
        witness(`header.parameters[${i}].type`, at, at + type.length, type),
      );
      witnesses.push(
        witness(
          `header.parameters[${i}].name`,
          at + type.length + 1,
          at + type.length + 1 + value.length,
          value,
        ),
      );
      witnesses.push(
        witness(`signature.parameterTypes[${i}]`, at, at + type.length, type),
      );
    }
    for (const [i, base] of bases.entries()) {
      const at = Buffer.from(text).indexOf(base, begin);
      witnesses.push(witness(`header.bases[${i}]`, at, at + base.length, base));
    }
    decls.push({
      ref,
      nativeId: null,
      role: "production",
      document: doc,
      revisionId: "r1",
      parentRef,
      kind,
      name,
      range: span(begin, end),
      nameRange: span(nameAt, nameAt + name.length),
      header,
      signature: params.length
        ? {
            parameterTypes: params.map(([type]) => type),
            typeParameterCount: 0,
            variadic: false,
          }
        : null,
      witnesses,
    });
  };
  const makeRef = (name, start, owner = "drive") => {
    const ref = `ref-${refs.length}`;
    refs.push({
      ref,
      nativeId: null,
      document: doc,
      revisionId: "r1",
      ownerRef: owner,
      range: span(start, start + name.length),
      spelling: name,
      witnesses: [witness("spelling", start, start + name.length, name)],
    });
    return ref;
  };
  const addFact = (kind, selector, record, extra = {}) => {
    const ref = `fact-${facts.length}`,
      proof = `proof-${ref}`;
    facts.push({
      kind,
      ref,
      ...(selector ? { anchor: selector } : {}),
      ...(kind === "typeRelationship"
        ? { ...extra, provenanceRef: proof }
        : { record, ...extra }),
    });
    return ref;
  };
  const host = add("class Host {\n"),
    targetStart = add(" void target() {}\n");
  simple(
    "target",
    "method",
    "target",
    targetStart,
    Buffer.byteLength(text) - 1,
    targetStart + 6,
    "host",
  );
  const driveStart = add(" void drive() {\n");
  const callSelectors = [];
  for (let i = 0; i < 120; i++) {
    const name = i < 4 ? "drive" : "target",
      start = add(`  ${name}();\n`) + 2;
    calls.push({
      ref: `call-${i}`,
      nativeId: null,
      document: doc,
      revisionId: "r1",
      ownerRef: "drive",
      range: span(start, start + name.length + 3),
      calleeRange: span(start, start + name.length),
      spelling: name,
      regionRefs: [],
      witnesses: [witness("spelling", start, start + name.length, name)],
    });
    callSelectors.push(anchor("callee", start, start + name.length, "drive"));
    if (i >= 4 && i < 23) makeRef(name, start);
  }
  const refSelectors = [];
  for (let i = 0; i < 20; i++) {
    const start = add("  target;\n") + 2;
    makeRef("target", start);
    refSelectors.push(anchor("reference", start, start + 6, "drive"));
  }
  const unsupportedSelectors = [];
  for (let i = 0; i < 20; i++) {
    const start = add("  target;\n") + 2;
    unsupportedSelectors.push(anchor("invocation", start, start + 7, "drive"));
  }
  add(" }\n");
  simple(
    "drive",
    "method",
    "drive",
    driveStart,
    Buffer.byteLength(text) - 1,
    driveStart + 6,
    "host",
  );
  for (let i = 0; i < 4; i++) {
    const name = `over${i}`;
    for (const [j, type] of ["int", "String"].entries()) {
      const start = add(` void ${name}(${type} value) {}\n`);
      simple(
        `over-${i}-${j}`,
        "method",
        name,
        start,
        Buffer.byteLength(text) - 1,
        start + 6,
        "host",
        [[type, "value"]],
      );
    }
  }
  add("}\n");
  simple("host", "type", "Host", host, Buffer.byteLength(text) - 1, host + 6);
  const other = add("class Other {\n"),
    otherMethod = add(" void over0(long value) {}\n");
  simple(
    "other-over0",
    "method",
    "over0",
    otherMethod,
    Buffer.byteLength(text) - 1,
    otherMethod + 6,
    "other",
    [["long", "value"]],
  );
  add("}\n");
  simple(
    "other",
    "type",
    "Other",
    other,
    Buffer.byteLength(text) - 1,
    other + 6,
  );
  const relationshipSelectors = [];
  for (let i = 0; i < 20; i++) {
    const name = `Child${i}`,
      base = `Base${i}`;
    const start = add(`class ${name} extends ${base} {}\n`);
    simple(
      `child-${i}`,
      "type",
      name,
      start,
      Buffer.byteLength(text) - 1,
      start + 6,
      null,
      [],
      [base],
    );
    relationshipSelectors.push(
      anchor(
        "declarationName",
        start + 6,
        start + 6 + name.length,
        `child-${i}`,
      ),
    );
    const baseStart = add(`class ${base} {}\n`);
    simple(
      `base-${i}`,
      "type",
      base,
      baseStart,
      Buffer.byteLength(text) - 1,
      baseStart + 6,
    );
    addFact("typeRelationship", null, null, {
      source: {
        kind: "internal",
        declarationRef: `child-${i}`,
        revisionId: "r1",
      },
      target: {
        kind: "internal",
        declarationRef: `base-${i}`,
        revisionId: "r1",
      },
      relationshipKind: "extends",
    });
  }
  const relationFacts = facts.map((x) => x.ref);
  const targetRef = {
      kind: "internal",
      declarationRef: "target",
      revisionId: "r1",
    },
    driveRef = { kind: "internal", declarationRef: "drive", revisionId: "r1" };
  const external = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "pkg external",
      scope: "global",
      document: null,
    },
  };
  const assertions = [];
  const refFacts = [];
  // Nineteen measured callees and twenty measured callable reads plus one definition.
  for (let i = 0; i < 39; i++) {
    const selector =
      i < 19
        ? { ...callSelectors[i + 4], kind: "reference" }
        : refSelectors[i - 19];
    const roles =
      i < 4
        ? ["read", "call", "import"]
        : i < 19
          ? ["read", "call"]
          : i === 19
            ? ["read", "write"]
            : i === 20
              ? ["read", "type"]
              : ["read"];
    const removed = omitRole === "read" && i !== 19 ? null : omitRole;
    const record = {
      site: "use",
      roles: roles.filter((role) => role !== removed),
      resolution: "resolved",
      declaredTarget: targetRef,
      candidates: [],
      provenanceId: "",
    };
    const fact = addFact("reference", selector, record);
    refFacts.push(fact);
    if (i >= 19)
      assertions.push({
        kind: "resolution",
        factRef: fact,
        disposition: "resolved",
      });
  }
  for (const [group, resolution] of [
    "external",
    "ambiguous",
    "unresolved",
  ].entries())
    for (let i = 0; i < 20; i++) {
      const selector = callSelectors[24 + group * 20 + i];
      const fact = addFact("callBinding", selector, {
        resolution,
        declaredTarget: resolution === "external" ? external : null,
        candidates: resolution === "ambiguous" ? [targetRef, external] : [],
        dispatch: "direct",
        possibleDispatch: [],
        possibleDispatchComplete: false,
        provenanceId: "",
      });
      assertions.push({
        kind: "resolution",
        factRef: fact,
        disposition: resolution === "external" ? "provenExternal" : resolution,
      });
    }
  if (omitRole === "definition") {
    // Keep forty measured uses: replace the declaration-site proof with a real unused use span.
    const start = unsupportedSelectors[0].range.start;
    makeRef("target", start);
    addFact("reference", anchor("reference", start, start + 6, "drive"), {
      site: "use",
      roles: ["read"],
      resolution: "resolved",
      declaredTarget: targetRef,
      candidates: [],
      provenanceId: "",
    });
  } else {
    makeRef("target", targetStart + 6, "host");
    addFact(
      "reference",
      anchor("reference", targetStart + 6, targetStart + 12, "host"),
      {
        site: "declaration",
        roles: ["definition"],
        resolution: "resolved",
        declaredTarget: targetRef,
        candidates: [],
        provenanceId: "",
      },
    );
  }
  const callFacts = [];
  for (let i = 0; i < 4; i++) {
    const fact = addFact("callBinding", callSelectors[i], {
      resolution: "resolved",
      declaredTarget: driveRef,
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "",
    });
    callFacts.push(fact);
  }
  const unsupportedFacts = [];
  for (let i = 0; i < (withoutUnsupportedFacts ? 0 : 20); i++) {
    const fact = addFact("callBinding", unsupportedSelectors[i], {
      resolution: "unresolved",
      declaredTarget: null,
      candidates: [],
      dispatch: "unknown",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "",
    });
    unsupportedFacts.push(fact);
    assertions.push({
      kind: "join",
      factRef: fact,
      disposition: "unsupported",
    });
  }
  const overloadFacts = [];
  for (let i = 0; i < 4; i++) {
    const row = decls.find((d) => d.ref === `over-${i}-1`);
    overloadFacts.push(
      addFact(
        "declarationBinding",
        anchor(
          "declarationName",
          row.nameRange.start,
          row.nameRange.end,
          "host",
        ),
        {
          symbols: [
            {
              scheme: "scip",
              symbol: `pkg ${row.name} String`,
              scope: "global",
              document: null,
            },
          ],
          provenanceId: "",
        },
      ),
    );
  }
  const overloadAnchors = [];
  for (let i = 0; i < 4; i++)
    overloadAnchors.push(
      [0, 1].map((j) => {
        const row = decls.find((d) => d.ref === `over-${i}-${j}`);
        return anchor(
          "declarationName",
          row.nameRange.start,
          row.nameRange.end,
          "host",
        );
      }),
    );
  const scenario = (category, anchors, factRefs) =>
    scenarios.push({
      id: `scenario-${scenarios.length}`,
      category,
      anchors,
      factRefs,
    });
  for (let i = 0; i < 4; i++) {
    scenario("sameNameOverload", overloadAnchors[i], [overloadFacts[i]]);
    scenario(
      "importsAliases",
      [{ ...callSelectors[i + 4], kind: "reference" }],
      [refFacts[i]],
    );
    scenario(
      "callableValues",
      Array.from({ length: 5 }, (_, j) => refSelectors[i * 5 + j]),
      Array.from({ length: 5 }, (_, j) => refFacts[19 + i * 5 + j]),
    );
    scenario("recursion", [callSelectors[i]], [callFacts[i]]);
    scenario(
      "relationshipsDispatch",
      [relationshipSelectors[i]],
      [relationFacts[i]],
    );
    scenario(
      "unicodeCoordinates",
      [{ ...callSelectors[i + 8], kind: "reference" }],
      [refFacts[i + 4]],
    );
    scenario(
      "coverageFreshness",
      [{ ...callSelectors[i + 12], kind: "reference" }],
      [refFacts[i + 8]],
    );
    scenario(
      "compatibilityControl",
      [{ ...callSelectors[i + 16], kind: "reference" }],
      [refFacts[i + 12]],
    );
  }
  const negatives = Array.from({ length: 20 }, (_, i) => ({
    scenarioId: scenarios[Math.floor(i / 5) * 8 + 2].id,
    referenceRef: refFacts[i + 19],
    ownerRef: "drive",
    range: refSelectors[i].range,
  }));
  const s = await specimen(t, {
    normalized: true,
    documentOverride: doc,
    profile: "corpus",
    sourceOverride: text,
    facts,
    scenarios,
    twoProducers: duplicateProducer,
    selectSecond: duplicateProducer,
    includeSecondInComparison: duplicateProducer,
    dispositions: {
      formatVersion: 1,
      assertions,
      callableValueNegatives: negatives,
    },
    mutateNative: (rows) => {
      rows.declarations = decls;
      rows.calls = calls;
      rows.references = refs;
      rows.controls = [];
    },
    mutateSupport: (support) => {
      if (allFamiliesAvailable) return;
      support.find((x) => x.kind === "invocation").available = false;
      support.find((x) => x.kind === "invocation").diagnostic =
        "invocation unavailable";
    },
    requestedRoles: ["definition", "read", "write", "call", "type", "import"],
    comparisonSemanticVersion: "2",
    mutateCoverage: (row) => ({
      ...row,
      state: row.selected ? "complete" : "omitted",
      supportedRoles: ["definition", "read", "write", "call", "type", "import"],
      observedRoles: row.selected
        ? ["definition", "read", "write", "call", "type", "import"]
        : [],
      diagnostic: row.selected ? null : "producer not selected",
    }),
  });
  return s;
}

test("measured same-name Java sibling category does not require a signature heuristic", async (t) => {
  const s = await generatedCorpus(t, { noOverloadSignatures: true });
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory.find(
      (row) => row.category === "sameNameOverload",
    ).count,
    4,
  );
});

test("temporary captured Java corpus reaches each exact floor through checkCounts", async (t) => {
  const s = await generatedCorpus(t);
  const count = checkCounts(s.loaded, s.records);
  assert.equal(count.scenariosTotal, 32);
  assert.deepEqual(
    count.scenariosByCategory.map((x) => x.count),
    Array(8).fill(4),
  );
  assert.equal(count.measuredCalls, 120);
  assert.equal(count.references, 40);
  assert.equal(count.callableValueNegatives, 20);
  assert.equal(count.typeRelationships, 20);
  // Twenty unsupported anchors share one document, so they count once.
  assert.deepEqual(
    count.outcomes.map((x) => x.count),
    [20, 20, 20, 20, 1],
  );
  assert.deepEqual(count.observedRoles, [
    "definition",
    "read",
    "write",
    "call",
    "type",
    "import",
  ]);
});

test("captured Java corpus checks independently removable role floors", async (t) => {
  const baseline = await generatedCorpus(t);
  const roles = ["definition", "write", "call", "type"];
  const variants = await Promise.all(
    roles.map((role) => generatedCorpus(t, { omitRole: role })),
  );
  const controls = registerControls(
    roles.map((role, i) => ({
      id: `COUNT.corpus.role-${role}`,
      baseline: () => 0,
      // Switch the complete admitted capture and normalized snapshot, not an output role list.
      mutate: () => 1,
      check: (value) => {
        const snapshot = value === 0 ? baseline : variants[i];
        return checkCounts(snapshot.loaded, snapshot.records);
      },
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    })),
  );
  for (const control of controls) await runControl(control);
});

test("captured Java role dependencies fail before the floor for read and import", async (t) => {
  const baseline = await generatedCorpus(t);
  const roles = ["read", "import"];
  const variants = await Promise.all(
    roles.map((role) => generatedCorpus(t, { omitRole: role })),
  );
  // Remove read from one witnessed callable-value negative: the other four reads keep
  // its scenario valid, but that negative must fail. Four Java importsAliases scenarios
  // require import, because alias is not an applicable Java role.
  const controls = registerControls(
    roles.map((role, i) => ({
      id: `COUNT.corpus.role-dependency-${role}`,
      baseline: () => 0,
      mutate: () => 1,
      check: (value) => {
        const snapshot = value === 0 ? baseline : variants[i];
        return checkCounts(snapshot.loaded, snapshot.records);
      },
      expectedAssertion: role === "read" ? "COUNT.NEGATIVE" : "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: role === "read" ? "callableValueNegatives" : "category",
    })),
  );
  for (const control of controls) await runControl(control);
});

test("integrated captured corpus rejects each count below its exact floor", async (t) => {
  const s = await generatedCorpus(t);
  const baseline = () => ({
    native: s.loaded.native,
    annotations: s.loaded.annotations,
    dispositions: s.loaded.dispositions,
    records: s.records,
  });
  const controls = registerControls(
    [
      {
        id: "COUNT.corpus.scenario-31",
        mutate: (value) => {
          value.annotations[0].scenarios.pop();
          return value;
        },
      },
      {
        id: "COUNT.corpus.negative-19",
        mutate: (value) => {
          value.dispositions.callableValueNegatives.pop();
          return value;
        },
      },
      {
        id: "COUNT.corpus.relationship-19",
        mutate: (value) => {
          const fact = value.annotations[0].facts
            .filter((x) => x.kind === "typeRelationship")
            .at(-1);
          value.annotations[0].facts = value.annotations[0].facts.filter(
            (x) => x !== fact,
          );
          value.records.typeRelationships =
            value.records.typeRelationships.filter(
              (x) => x.provenanceId !== fact.provenanceRef,
            );
          return value;
        },
      },
      {
        id: "COUNT.corpus.reference-39",
        mutate: (value) => {
          const fact = value.annotations[0].facts.find(
            (x) => x.kind === "reference" && x.record.site === "declaration",
          );
          value.annotations[0].facts = value.annotations[0].facts.filter(
            (x) => x !== fact,
          );
          value.records.references = value.records.references.filter(
            (x) => x.provenanceId !== fact.record.provenanceId,
          );
          return value;
        },
      },
      {
        id: "COUNT.corpus.calls-119",
        mutate: (value) => {
          const native = value.native.calls.pop();
          value.records.calls = value.records.calls.filter(
            (x) => x.range.start !== native.range.start,
          );
          return value;
        },
      },
      ...[
        "sameNameOverload",
        "importsAliases",
        "callableValues",
        "recursion",
        "relationshipsDispatch",
        "unicodeCoordinates",
        "coverageFreshness",
        "compatibilityControl",
      ].map((category) => ({
        id: `COUNT.corpus.category-${category}-3`,
        mutate: (value) => {
          const rows = value.annotations[0].scenarios;
          const [removed] = rows.splice(
            rows.findIndex((x) => x.category === category),
            1,
          );
          value.dispositions.callableValueNegatives =
            value.dispositions.callableValueNegatives.filter(
              (x) => x.scenarioId !== removed.id,
            );
          return value;
        },
      })),
      // Java has no ambiguous floor (Decision 0017 Amendment 1).
      ...["resolved", "provenExternal", "unresolved"].map(
        (disposition) => ({
          id: `COUNT.corpus.outcome-${disposition}-19`,
          mutate: (value) => {
            const assertions = value.dispositions.assertions;
            assertions.splice(
              assertions.findIndex((x) => x.disposition === disposition),
              1,
            );
            return value;
          },
        }),
      ),
    ].map((row) => ({
      ...row,
      baseline,
      check: (value) =>
        checkCounts(
          {
            ...s.loaded,
            native: value.native,
            annotations: value.annotations,
            dispositions: value.dispositions,
          },
          value.records,
        ),
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    })),
  );
  for (const control of controls) await runControl(control);
});

test("captured Java corpus with no ambiguous outcomes meets its floors", async (t) => {
  const s = await generatedCorpus(t);
  const dispositions = structuredClone(s.loaded.dispositions);
  dispositions.assertions = dispositions.assertions.filter(
    (x) => x.disposition !== "ambiguous",
  );
  const count = checkCounts({ ...s.loaded, dispositions }, s.records);
  assert.equal(
    count.outcomes.find((x) => x.disposition === "ambiguous").count,
    0,
  );
  assert.equal(count.floorsEnforced, true);
});

test("unresolved outcome counts only in a complete-coverage document", async (t) => {
  // A measured call with no producer target, captured as unresolved.
  const unresolvedCall = (fact) => {
    fact.record.resolution = "unresolved";
    fact.record.declaredTarget = null;
    fact.record.candidates = [];
  };
  const complete = await specimen(t, {
    family: "callee",
    normalized: true,
    mutateFact: unresolvedCall,
  });
  // Same call, but the requested read role is unmeasurable, so the semantic
  // tuple is partial and absence of a target is not evidence.
  const partial = await specimen(t, {
    family: "callee",
    normalized: true,
    mutateFact: unresolvedCall,
    mutateSupport: (support) => {
      const reference = support.find((x) => x.kind === "reference");
      reference.available = false;
      reference.diagnostic = "reference unavailable";
    },
    mutateCoverage: (row) =>
      row.selected
        ? { ...row, state: "partial", diagnostic: "reference unavailable" }
        : row,
  });
  for (const s of [complete, partial])
    s.loaded.dispositions.assertions.push({
      kind: "resolution",
      factRef: "fact",
      disposition: "unresolved",
    });
  assert.equal(
    checkCounts(complete.loaded, complete.records).outcomes.find(
      (x) => x.disposition === "unresolved",
    ).count,
    1,
  );
  const controls = registerControls([
    {
      id: "COUNT.disposition.unresolved-partial-coverage",
      baseline: () => 0,
      mutate: () => 1,
      check: (value) => {
        const s = value === 0 ? complete : partial;
        return checkCounts(s.loaded, s.records);
      },
      expectedAssertion: "COUNT.DISPOSITION",
      expectedCode: "invalidRecord",
      expectedField: "assertions",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("unsupported floor: a declared-unavailable native family plus producer facts qualifies the document", async (t) => {
  // Twenty unsupported-join assertions in one document count once.
  const full = await generatedCorpus(t);
  const unsupported = (s) =>
    checkCounts(s.loaded, s.records).outcomes.find(
      (x) => x.disposition === "unsupported",
    ).count;
  assert.equal(unsupported(full), 1);
  // No invocation-family facts and no unsupported assertions: the unavailable
  // native invocation capability and the document's other producer facts are
  // enough (Prime's PR #268 repro).
  const capabilityOnly = await generatedCorpus(t, {
    withoutUnsupportedFacts: true,
  });
  assert.equal(
    capabilityOnly.loaded.dispositions.assertions.some(
      (x) => x.disposition === "unsupported",
    ),
    false,
  );
  assert.equal(unsupported(capabilityOnly), 1);
  assert.equal(
    checkCounts(capabilityOnly.loaded, capabilityOnly.records).floorsEnforced,
    true,
  );
  // A document whose native families are all available does not qualify.
  const allAvailable = await generatedCorpus(t, {
    withoutUnsupportedFacts: true,
    allFamiliesAvailable: true,
  });
  const controls = registerControls([
    {
      id: "COUNT.corpus.unsupported-no-qualifying-document",
      baseline: () => 0,
      mutate: () => 1,
      check: (value) => {
        const s = value === 0 ? capabilityOnly : allAvailable;
        return checkCounts(s.loaded, s.records);
      },
      expectedAssertion: "COUNT.FLOOR",
      expectedCode: "invalidRecord",
      expectedField: "counts",
    },
  ]);
  for (const control of controls) await runControl(control);
});

test("Java overloads accept distinct signatures but reject unrelated names and containers", async (t) => {
  const s = await generatedCorpus(t);
  const declarations = s.loaded.native.declarations;
  const a = declarations.find((x) => x.ref === "over-0-0"),
    b = declarations.find((x) => x.ref === "over-0-1");
  assert.deepEqual(
    [a.signature.parameterTypes, b.signature.parameterTypes],
    [["int"], ["String"]],
  );
  assert.equal(
    checkCounts(s.loaded, s.records).scenariosByCategory[0].count,
    4,
  );
  const additional = (ref) => {
    const row = declarations.find((x) => x.ref === ref);
    return {
      document: row.document,
      revisionId: row.revisionId,
      contentHash: s.loaded.annotations[0].scenarios[0].anchors[0].contentHash,
      kind: "declarationName",
      range: row.nameRange,
      ownerRef: row.parentRef ?? row.ref,
    };
  };
  const controls = registerControls(
    [
      { id: "COUNT.overload.unrelated-name", extra: additional("over-1-0") },
      {
        id: "COUNT.overload.unrelated-container",
        extra: additional("other-over0"),
      },
    ].map(({ id, extra }) => ({
      id,
      baseline: () => s.loaded.annotations,
      mutate: (annotations) => {
        annotations[0].scenarios[0].anchors.push(extra);
        return annotations;
      },
      check: (annotations) =>
        checkCounts({ ...s.loaded, annotations }, s.records),
      expectedAssertion: "COUNT.SCENARIO",
      expectedCode: "invalidRecord",
      expectedField: "category",
    })),
  );
  for (const control of controls) await runControl(control);
});

test("second captured producer cannot duplicate a directed edge at the corpus floor", async (t) => {
  const s = await generatedCorpus(t, { duplicateProducer: true });
  assert.equal(s.records.typeRelationships.length, 21);
  assert.equal(checkCounts(s.loaded, s.records).typeRelationships, 20);
});
