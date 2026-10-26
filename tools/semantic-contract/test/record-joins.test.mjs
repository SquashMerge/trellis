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
import { normalizeFixture } from "../normalize.mjs";
import { applicableRoles } from "../lookup.mjs";
import { registerControls, runControl } from "./mutations.mjs";

// Independently authored source offsets, digest inputs and expected normalized rows.
const source =
  "function main() { target(); callback; }\nfunction target() {}\n";
const alternateSource = (kind) => `${source}// admitted ${kind} snapshot\n`;
const document = {
  sourceSetId: "main",
  language: "javascript",
  path: "src/main.js",
};
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
  capabilityReports: [],
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
    ambiguousNative = false,
  } = {},
) {
  const root = await mkdtemp(join(tmpdir(), "join-u3-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const fs = new Map(),
    put = (path, value) =>
      fs.set(path, typeof value === "string" ? value : JSON.stringify(value));
  const shifted = emojiPrefix || encoding !== "utf8",
    offset = shifted ? 5 : 0,
    positionOffset =
      encoding === "utf16" ? 3 : encoding === "unicodeScalar" ? 2 : offset;
  const sourceText = shifted ? "😀\n" + source : source,
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
  if (ambiguousNative) {
    const child = declaration(
      "nested-target",
      "target",
      49 + offset,
      59 + offset,
      49 + offset,
    );
    child.parentRef = "target";
    child.kind = "field";
    child.header = { ...header("target"), kind: "field" };
    measuredNative.declarations.push(child);
  }
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
  const authored = structuredClone(facts ?? [makeFact(family, status)]);
  for (const fact of authored) {
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
  for (let i = 1; i < authored.length; i++)
    authored[i].record.provenanceId = `proof-${authored[i].ref}`;
  if (twoProducers) {
    const other = structuredClone(authored[0]);
    other.ref = "fact-b";
    other.record.provenanceId = "proof-b";
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
    language: "javascript",
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
  const proofs = authored.map((fact) =>
    fact.ref === "fact-b"
      ? {
          ...proof,
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
      : { ...proof, id: fact.record.provenanceId },
  );
  const proofFacts = proofs.map((record, i) => ({
    kind: "provenance",
    ref: `proof-fact-${i}`,
    record,
  }));
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({
      kind,
      gaps: [], available: !(status === "unsupported" && kind === unsupportedFamily),
      diagnostic:
        status === "unsupported" && kind === unsupportedFamily
          ? "native family unavailable"
          : null,
    }),
  );
  mutateSupport?.(support);
  const requestedRoles = [
    ...new Set([
      "read",
      ...authored.flatMap((fact) =>
        fact.kind === "reference"
          ? fact.record.roles
          : fact.kind === "callBinding"
            ? ["call"]
            : [],
      ),
    ]),
  ]
    .filter((role) => applicableRoles(document.language).includes(role))
    .sort(
      (a, b) =>
        applicableRoles(document.language).indexOf(a) -
        applicableRoles(document.language).indexOf(b),
    );
  const availableRole = (role) =>
    (role === "call"
      ? ["reference", "invocation", "callee"]
      : role === "definition" || role === "alias"
        ? ["reference", "declarationName"]
        : ["reference"]
    ).every(
      (kind) => support.find((x) => x.kind === kind)?.available !== false,
    );
  const observedRoles = requestedRoles.filter(availableRole),
    missingRoles = observedRoles.length !== requestedRoles.length;
  const coverage = (producerId) => {
    const selected = producerId !== "semantic-b" || selectSecond;
    return {
      producerId,
      language: document.language,
      sourceSetId: document.sourceSetId,
      documentPath: document.path,
      revisionId: "r1",
      requested: true,
      selected,
      state: !selected ? "omitted" : missingRoles ? "partial" : "complete",
      supportedRoles: requestedRoles,
      observedRoles: selected ? observedRoles : [],
      diagnostic: !selected
        ? "producer not selected"
        : missingRoles
          ? "requested role unavailable"
          : null,
    };
  };
  put("snapshots/src/main.js.annotations.json", {
    formatVersion: 1,
    document,
    revisionId: "r1",
    scenarios: [],
    facts: [
      ...(twoProducers
        ? ["native", "semantic", "semantic-b"]
        : ["native", "semantic"]
      ).map((id) => ({
        kind: "coverage",
        ref: `coverage-${id}`,
        record: coverage(id),
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
    positionEncoding: encoding,
  };
  const producers = twoProducers
    ? [nativeProducer, selectedSemantic, unselectedSemantic]
    : [nativeProducer, selectedSemantic];
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
    producers,
    revisions: [revision],
    comparison: {
      sourceSetId: "main",
      revisionId: "r1",
      producers: [nativeProducer, selectedSemantic],
    },
    coverageIntents: producers.map((producer) => ({
      producerId: producer.id,
      document,
      revisionId: "r1",
      requestedRoles,
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
      languages: ["javascript"],
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
          requestedRoles,
          measurementSupport: structuredClone(support),
        });
    }
    // The captured semantic proof's manifest includes both documents of r1/main.
  }
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
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
  if (ambiguousNative) {
    const childKey = {
      kind: "field",
      name: "target",
      signature: null,
      ordinal: 0,
    };
    const parentKey = {
      kind: "function",
      name: "target",
      signature: null,
      ordinal: 0,
    };
    const childId = `sid:v1:${domain("syntax", { sourceSet: "main", path: document.path, language: "javascript", ancestors: [parentKey], declaration: childKey })}`;
    records.declarations.push({
      syntaxId: childId,
    role: "production",
      document,
      revisionId: "r1",
      kind: "field",
      name: "target",
      lookupKey: "target",
      ancestors: [parentKey],
      key: childKey,
      range: range(49 + offset, 59 + offset),
      nameRange: range(49 + offset, 55 + offset),
      header: { ...header("target"), kind: "field" },
      provenanceId: `native:r1:${childId}`,
    });
    records.declarations.sort((a, b) =>
      Buffer.compare(Buffer.from(a.syntaxId), Buffer.from(b.syntaxId)),
    );
  }
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
  if (ambiguousNative) {
    const child = records.declarations.find((row) => row.kind === "field");
    for (const row of expectedJoin)
      if (row.anchor.kind === "declarationName" && row.status === "exact") {
        row.status = "ambiguous";
        row.candidateIds = [targetId, child.syntaxId].sort((a, b) =>
          Buffer.compare(Buffer.from(a), Buffer.from(b)),
        );
        row.diagnostic = "ambiguous";
      }
  }
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
function check(s) {
  return checkJoins(s.loaded, s.records, s.coverageResult, s.measurement);
}

test("admitted source bytes produce exact/unmatched/unsupported joins for all four families", async (t) => {
  for (const family of ["declarationName", "callee", "invocation", "reference"])
    for (const status of ["exact", "unmatched", "unsupported"]) {
      const s = await specimen(t, { family, status }),
        out = check(s);
      assert.equal(
        canonical(out.joined.get("fact").join),
        canonical(s.expectedJoin[0]),
        `${family} ${status}`,
      );
      if (family === "reference")
        assert.equal(s.records.references.length, status === "exact" ? 1 : 0);
    }
});
// Each control reloads authored bytes before validation. The cloned control state is
// only a switch; it never mutates a loaded raw artifact after admission.
async function control(
  t,
  {
    name,
    options = {},
    raw = null,
    normalized = null,
    nativeMutation = null,
    supportMutation = null,
    changedOptions = {},
    assertion,
    field,
    code = "invalidRecord",
  },
) {
  const [row] = registerControls([
    {
      id: `U3.${name}`,
      baseline: () => ({ changed: false }),
      mutate: (state) => ({ ...state, changed: true }),
      check: async (state) => {
        const changed = state.changed;
        const s = await specimen(t, {
          ...options,
          ...(changed ? changedOptions : {}),
          mutateFact:
            options.mutateFact || (changed && raw)
              ? (fact) => {
                  options.mutateFact?.(fact);
                  if (changed) raw?.(fact);
                }
              : null,
          mutateNative: changed ? nativeMutation : null,
          mutateSupport: changed ? supportMutation : null,
        });
        if (changed) normalized?.(s.records, s.measurement);
        checkCoverage(s.loaded, s.records);
        checkMeasurement(s.loaded, s.records);
        return check(s);
      },
      expectedAssertion: assertion,
      expectedCode: code,
      expectedField: field,
    },
  ]);
  return runControl(row);
}

test("per-fact installed identity, source proof, native refs, and diagnostic-only mapping", async (t) => {
  for (const family of ["declarationName", "callee", "invocation", "reference"])
    for (const status of ["exact", "unmatched", "unsupported"]) {
      const s = await specimen(t, { family, status }),
        result = check(s),
        row = result.joined.get("fact");
      assert.equal(canonical(row.join), canonical(s.expectedJoin[0]));
      assert.equal(
        row.installedId,
        status === "exact" ? s.expectedJoin[0].candidateIds[0] : null,
      );
      assert.equal(row.producerId, "semantic");
      assert.deepEqual(row.provenanceIds, ["proof"]);
      assert.deepEqual(
        row.nativeRefs,
        status === "exact"
          ? [
              {
                declarationName: "target",
                callee: "call",
                invocation: "call",
                reference: "target-ref",
              }[family],
            ]
          : [],
      );
      assert.equal(result.recordByFactRef.has("fact"), true);
      if (family === "reference") {
        assert.equal(
          result.expectedReferences.length,
          status === "exact" ? 1 : 0,
        );
        assert.equal(
          result.expectedDiagnostics.length,
          status === "exact" ? 0 : 1,
        );
        assert.equal(
          result.recordByFactRef.get("fact").factRef,
          status === "exact" ? undefined : "fact",
        );
      }
    }
});

test("native controls overlap invocations but cannot join any family", async (t) => {
  for (const family of ["callee", "invocation", "reference"]) {
    const s = await specimen(t, { family, control: true }),
      result = check(s);
    assert.equal(
      s.loaded.native.controls[0].range.start,
      s.loaded.native.calls[0].range.start,
    );
    assert.equal(
      result.joined.get("fact").installedId,
      s.expectedJoin[0].candidateIds[0],
    );
    assert.equal(
      result.joined.get("fact").nativeRefs.includes("region"),
      false,
    );
  }
});

test("real emoji prefix maps UTF-8, UTF-16 and scalar positions to the same native byte span", async (t) => {
  const rows = [];
  for (const encoding of ["utf8", "utf16", "unicodeScalar"]) {
    const s = await specimen(t, { encoding, emojiPrefix: true }),
      actual = check(s).joined.get("fact");
    rows.push(actual.join.anchor.range);
    assert.deepEqual(actual.join.anchor.range, { start: 23, end: 29 });
    assert.deepEqual(actual.nativeRefs, ["target-ref"]);
  }
  assert.deepEqual(rows, [
    { start: 23, end: 29 },
    { start: 23, end: 29 },
    { start: 23, end: 29 },
  ]);
});

test("bad byte/scalar boundaries and tuple, family, owner, support facts", async (t) => {
  const rawCases = [
    [
      "hash",
      {},
      (f) => {
        f.anchor.contentHash = sha("wrong");
      },
      "JOIN.TUPLE",
      "anchor",
    ],
    [
      "encoding",
      {},
      (f) => {
        f.anchor.range.encoding = "utf16";
      },
      "JOIN.TUPLE",
      "anchor.range",
    ],
    [
      "family",
      {},
      (f) => {
        f.anchor.kind = "callee";
      },
      "JOIN.FAMILY",
      "anchor.kind",
    ],
    [
      "owner absent",
      {},
      (f) => {
        f.anchor.ownerRef = "absent";
      },
      "JOIN.OWNER",
      "anchor.ownerRef",
    ],
    [
      "owner measured but wrong",
      {},
      (f) => {
        f.anchor.ownerRef = "target";
      },
      "JOIN.OWNER",
      "anchor.ownerRef",
    ],
    [
      "utf8 split",
      { encoding: "utf8", emojiPrefix: true },
      (f) => {
        f.anchor.range = span(1, 4);
      },
      "JOIN.TUPLE",
      "anchor.range",
      "invalidRange",
    ],
    [
      "utf16 surrogate",
      { encoding: "utf16" },
      (f) => {
        f.anchor.range = span(1, 3, "utf16");
      },
      "JOIN.TUPLE",
      "anchor.range",
      "invalidRange",
    ],
    [
      "scalar out of bounds",
      { encoding: "unicodeScalar" },
      (f) => {
        f.anchor.range = span(1, 1000, "unicodeScalar");
      },
      "JOIN.TUPLE",
      "anchor.range",
      "invalidRange",
    ],
    [
      "scalar reversed",
      { encoding: "unicodeScalar" },
      (f) => {
        f.anchor.range = span(22, 20, "unicodeScalar");
      },
      "JOIN.TUPLE",
      "anchor.range",
      "invalidRange",
    ],
  ];
  for (const [name, options, raw, assertion, field, code] of rawCases)
    await control(t, { name, options, raw, assertion, field, code });
});

test("alternate admitted source tuples, native support and candidate inventory boundaries", async (t) => {
  const alternate = await specimen(t, { alternateTuples: true });
  assert.equal(check(alternate).joined.get("fact").installedId, refId);
  assert.equal(alternate.loaded.revisions.size, 3);
  assert.equal(
    alternate.loaded.revisions.get(JSON.stringify(["main", "r1"])).documents
      .length,
    2,
  );
  assert.equal(
    alternate.measurement.recordByNativeRef.get("target").syntaxId,
    targetId,
  );
  for (const name of ["document", "revision", "source set"])
    await control(t, {
      name: `admitted alternate ${name} hash`,
      options: { alternateTuples: true },
      raw: (fact) => {
        fact.anchor.contentHash = sha(alternateSource(name));
      },
      assertion: "JOIN.TUPLE",
      field: "anchor",
    });
  // Raw document and revision identity cannot bypass the loader's same-annotation
  // capture check; these controls exercise that earlier admitted-source boundary.
  for (const [name, raw] of [
    [
      "document",
      (f) => {
        f.anchor.document = { ...f.anchor.document, path: "src/other.js" };
      },
    ],
    [
      "revision",
      (f) => {
        f.anchor.revisionId = "r2";
      },
    ],
    [
      "source set",
      (f) => {
        f.anchor.document = { ...f.anchor.document, sourceSetId: "alternate" };
      },
    ],
  ])
    await control(t, {
      name: `loader alternate ${name}`,
      options: { alternateTuples: true },
      raw,
      assertion: "IDENTITY.SEMANTIC",
      field: "fact",
    });
  await control(t, {
    name: "native family falsely unavailable",
    options: { family: "callee" },
    supportMutation: (support) => {
      const family = support.find((x) => x.kind === "callee");
      family.available = false;
      family.diagnostic = "callee unavailable";
    },
    assertion: "JOIN.SUPPORT",
    field: "measurementSupport",
  });
  await control(t, {
    name: "duplicate native candidate rejected upstream",
    nativeMutation: (artifact) => {
      artifact.references.push({
        ...structuredClone(artifact.references[0]),
        ref: "duplicate-ref",
      });
    },
    assertion: "ID.ORDINAL",
    field: "range",
  });
  const unmatched = await specimen(t, { status: "unmatched" });
  const before = check(unmatched).joined.get("fact");
  unmatched.measurement.candidateRows.push({
    ...unmatched.measurement.candidateRows[0],
    anchor: before.join.anchor,
    ownerRef: "main",
    id: refId,
    ref: "target-ref",
  });
  const after = check(unmatched).joined.get("fact");
  assert.deepEqual(after, before);
  assert.equal(after.installedId, null);
});

test("reference site, roles and measured identity require independently admitted source", async (t) => {
  const declaration = await specimen(t, { declarationSite: true });
  assert.equal(check(declaration).expectedReferences[0].site, "declaration");
  for (const [name, raw, assertion, field, options] of [
    [
      "empty roles",
      (f) => {
        f.record.roles = [];
      },
      "REFERENCE.ROLES",
      "roles",
      {},
    ],
    [
      "false declaration",
      (f) => {
        f.record.site = "declaration";
      },
      "REFERENCE.ROLES",
      "site",
      {},
    ],
    [
      "alias lacks definition",
      (f) => {
        f.record.roles = ["alias"];
      },
      "REFERENCE.ROLES",
      "site",
      {},
    ],
    [
      "misordered roles",
      (f) => {
        f.record.roles = ["call", "read"];
      },
      "REFERENCE.ROLES",
      "roles",
      {},
    ],
    [
      "false callback call",
      (f) => {
        f.record.roles = ["call"];
      },
      "REFERENCE.ROLES",
      "roles",
      { callbackSite: true },
    ],
    [
      "bad resolution cardinality",
      (f) => {
        f.record.resolution = "ambiguous";
      },
      "REFERENCE.RESOLUTION",
      "resolution",
      {},
    ],
    [
      "bad target",
      (f) => {
        f.record.declaredTarget.revisionId = "r2";
      },
      "REFERENCE.RESOLUTION",
      "declaredTarget",
      {},
    ],
  ]) {
    await control(t, { name, options, raw, assertion, field });
  }
  for (const field of [
    "id",
    "ownerSyntaxId",
    "ordinal",
    "range",
    "spelling",
    "lookupKey",
    "provenanceId",
  ]) {
    const value = {
      id: callbackId,
      ownerSyntaxId: targetId,
      ordinal: 1,
      range: { start: 28, end: 36 },
      spelling: "callback",
      lookupKey: "callback",
      provenanceId: "other",
    }[field];
    await control(t, {
      name: `measured reference ${field}`,
      normalized: (records) => {
        records.references[0][field] = value;
      },
      assertion: "REFERENCE.SOURCE",
      field: "references",
    });
  }
});

test("valid external, ambiguous semantic resolution, unresolved and target cardinality", async (t) => {
  const external = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "pkg external",
      scope: "global",
      document: null,
    },
  };
  for (const resolution of [
    "resolved",
    "external",
    "ambiguous",
    "unresolved",
  ]) {
    const options = {
      mutateFact: (f) => {
        f.record.resolution = resolution;
        f.record.declaredTarget =
          resolution === "resolved"
            ? declaredTarget
            : resolution === "external"
              ? external
              : null;
        f.record.candidates =
          resolution === "ambiguous" ? [declaredTarget, external] : [];
      },
    };
    const s = await specimen(t, options),
      result = check(s);
    assert.equal(result.expectedReferences[0].resolution, resolution);
    assert.equal(
      result.expectedReferences[0].candidates.length,
      resolution === "ambiguous" ? 2 : 0,
    );
  }
  const ambiguous = {
    mutateFact: (f) => {
      f.record.resolution = "ambiguous";
      f.record.declaredTarget = null;
      f.record.candidates = [declaredTarget, external];
    },
  };
  await control(t, {
    name: "duplicate targets",
    options: ambiguous,
    raw: (f) => {
      f.record.candidates = [declaredTarget, declaredTarget];
    },
    assertion: "REFERENCE.RESOLUTION",
    field: "candidates",
  });
  await control(t, {
    name: "unsorted targets",
    options: ambiguous,
    raw: (f) => {
      f.record.candidates = [external, declaredTarget];
    },
    assertion: "REFERENCE.RESOLUTION",
    field: "candidates",
  });
  await control(t, {
    name: "wrong target",
    options: ambiguous,
    raw: (f) => {
      f.record.candidates[0] = { ...declaredTarget, declarationRef: "missing" };
    },
    assertion: "REFERENCE.RESOLUTION",
    field: "declaredTarget",
  });
});

test("two distinct measured declaration identities at the same owner and anchor are ambiguous, not installed", async (t) => {
  const s = await specimen(t, {
    family: "declarationName",
    ambiguousNative: true,
  });
  const id = s.records.declarations.find(
    (row) => row.kind === "field",
  ).syntaxId;
  const expected = [targetId, id].sort((a, b) =>
    Buffer.compare(Buffer.from(a), Buffer.from(b)),
  );
  const result = check(s),
    joined = result.joined.get("fact");
  assert.equal(joined.join.status, "ambiguous");
  assert.deepEqual(joined.join.candidateIds, expected);
  assert.equal(joined.join.diagnostic, "ambiguous");
  assert.equal(joined.installedId, null);
  assert.deepEqual(joined.nativeRefs, []);
  assert.deepEqual(result.recordByFactRef.get("fact"), joined.join);
  const normalized = normalizeFixture(s.loaded).records.declarationBindings[0];
  assert.deepEqual(normalized.join, joined.join);
  assert.equal(normalized.syntaxId, null);
});

test("contradictory exact reference targets retain both proofs and reject forged first-wins", async (t) => {
  const external = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "pkg external",
      scope: "global",
      document: null,
    },
  };
  const facts = [
    makeFact("reference", "exact"),
    makeFact("reference", "exact", "fact2"),
  ];
  facts[1].record.resolution = "external";
  facts[1].record.declaredTarget = external;
  const s = await specimen(t, { facts }),
    candidates = ordered([target, external]);
  for (const row of s.records.references) {
    row.resolution = "ambiguous";
    row.declaredTarget = null;
    row.candidates = candidates;
  }
  s.records.references = ordered(s.records.references);
  const result = check(s);
  assert.equal(result.expectedReferences.length, 2);
  assert.deepEqual(
    result.expectedReferences.map((row) => row.provenanceId).sort(),
    ["proof", "proof-fact2"],
  );
  for (const row of result.expectedReferences) {
    assert.equal(row.resolution, "ambiguous");
    assert.equal(row.declaredTarget, null);
    assert.equal(canonical(row.candidates), canonical(candidates));
  }
  assert.equal(
    canonical(result.recordByFactRef.get("fact").candidates),
    canonical(candidates),
  );
  assert.equal(
    canonical(result.recordByFactRef.get("fact2").candidates),
    canonical(candidates),
  );
  const compilation = normalizeFixture(s.loaded),
    normalized = compilation.records.references;
  assert.equal(canonical(normalized), canonical(result.expectedReferences));
  for (const [factRef, proofId] of [
    ["fact", "proof"],
    ["fact2", "proof-fact2"],
  ]) {
    assert.equal(compilation.recordMap.get(factRef).provenanceId, proofId);
    assert.equal(compilation.recordMap.get(factRef).resolution, "ambiguous");
    assert.equal(
      canonical(compilation.recordMap.get(factRef).candidates),
      canonical(candidates),
    );
  }
  const wrong = structuredClone(s.records);
  wrong.references[0].resolution = "resolved";
  wrong.references[0].declaredTarget = target;
  wrong.references[0].candidates = [];
  wrong.references = ordered(wrong.references);
  assert.throws(
    () => checkJoins(s.loaded, wrong, s.coverageResult, s.measurement),
    (e) =>
      e.assertion === "REFERENCE.SOURCE" &&
      e.code === "invalidRecord" &&
      e.field === "references",
  );
});

test("identical repeated facts keep distinct proof contributors; conflicting facts reject", async (t) => {
  const facts = [
    makeFact("reference", "exact"),
    makeFact("reference", "exact", "fact2"),
  ];
  const s = await specimen(t, { facts }),
    result = check(s);
  assert.equal(result.joined.size, 2);
  assert.equal(result.expectedReferences.length, 2);
  assert.deepEqual(result.joined.get("fact2").provenanceIds, ["proof-fact2"]);
  const external = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "pkg external",
      scope: "global",
      document: null,
    },
  };
  for (const [name, mutate, options] of [
    [
      "roles",
      (f) => {
        f.record.roles = ["read"];
      },
      {},
    ],
    [
      "site",
      (f) => {
        f.record.site = "use";
      },
      {
        declarationSite: true,
        mutateFact: (f) => {
          f.record.roles = ["read"];
        },
      },
    ],
    [
      "resolution",
      (f) => {
        f.record.resolution = "unresolved";
        f.record.declaredTarget = null;
      },
      {},
    ],
    [
      "target",
      (f) => {
        f.record.resolution = "external";
        f.record.declaredTarget = external;
      },
      {},
    ],
  ])
    await control(t, {
      name: `conflicting ${name}`,
      options: { facts, ...options },
      raw: (f) => {
        if (f.ref === "fact2") mutate(f);
      },
      assertion: "REFERENCE.SOURCE",
      field: "references",
    });
});

test("exact Reference membership and fact-linked diagnostic are closed", async (t) => {
  for (const [name, normalized, assertion, field] of [
    [
      "exact reference omitted",
      (r) => {
        r.references = [];
      },
      "REFERENCE.SOURCE",
      "references",
    ],
    [
      "exact reference extra",
      (r) => {
        r.references.push({ ...r.references[0], id: callbackId });
        r.references = ordered(r.references);
      },
      "REFERENCE.SOURCE",
      "references",
    ],
    [
      "exact fact diagnostic",
      (r) => {
        r.referenceJoinDiagnostics.push({
          factRef: "fact",
          provenanceId: "proof",
          join: {
            anchor: {
              document,
              revisionId: "r1",
              contentHash,
              range: range(18, 24),
              kind: "reference",
            },
            status: "unmatched",
            candidateIds: [],
            diagnostic: "unmatched",
          },
        });
      },
      "JOIN.DIAGNOSTIC",
      "referenceJoinDiagnostics",
    ],
    [
      "exact fact proof swap",
      (r) => {
        r.references[0].provenanceId = "proof-other";
      },
      "REFERENCE.SOURCE",
      "references",
    ],
  ])
    await control(t, { name, normalized, assertion, field });
  for (const status of ["unmatched", "unsupported"])
    await control(t, {
      name: `${status} duplicate diagnostic`,
      options: { status },
      normalized: (r) => {
        r.referenceJoinDiagnostics.push(
          structuredClone(r.referenceJoinDiagnostics[0]),
        );
      },
      assertion: "JOIN.DIAGNOSTIC",
      field: "referenceJoinDiagnostics",
    });
});

test("diagnostics derive candidate and support state, never install unmatched reference", async (t) => {
  for (const status of ["unmatched", "unsupported"]) {
    for (const [name, normalized, assertion, field] of [
      [
        "missing",
        (r) => {
          r.referenceJoinDiagnostics = [];
        },
        "JOIN.DIAGNOSTIC",
        "referenceJoinDiagnostics",
      ],
      [
        "forged ambiguous",
        (r) => {
          r.referenceJoinDiagnostics[0].join.status = "ambiguous";
          r.referenceJoinDiagnostics[0].join.diagnostic = "ambiguous";
        },
        "JOIN.DIAGNOSTIC",
        "referenceJoinDiagnostics",
      ],
      [
        "candidate",
        (r) => {
          r.referenceJoinDiagnostics[0].join.candidateIds = [refId];
        },
        "JOIN.DIAGNOSTIC",
        "referenceJoinDiagnostics",
      ],
      [
        "status swap",
        (r) => {
          r.referenceJoinDiagnostics[0].join.status =
            status === "unmatched" ? "unsupported" : "unmatched";
          r.referenceJoinDiagnostics[0].join.diagnostic =
            status === "unmatched" ? "native family unavailable" : "unmatched";
        },
        "JOIN.DIAGNOSTIC",
        "referenceJoinDiagnostics",
      ],
      [
        "extra",
        (r) => {
          r.referenceJoinDiagnostics.push({
            ...r.referenceJoinDiagnostics[0],
            factRef: "extra",
          });
          r.referenceJoinDiagnostics.sort((a, b) =>
            Buffer.compare(Buffer.from(a.factRef), Buffer.from(b.factRef)),
          );
        },
        "JOIN.DIAGNOSTIC",
        "referenceJoinDiagnostics",
      ],
      [
        "install",
        (r) => {
          r.references.push({
            id: refId,
            ownerSyntaxId: mainId,
            ordinal: 0,
            document,
            revisionId: "r1",
            range: range(18, 24),
            spelling: "target",
            lookupKey: "target",
            site: "use",
            roles: ["read"],
            resolution: "unresolved",
            declaredTarget: null,
            candidates: [],
            provenanceId: "proof",
          });
        },
        "REFERENCE.SOURCE",
        "references",
      ],
    ])
      await control(t, {
        name: `${status} ${name}`,
        options: { status },
        normalized,
        assertion:
          status === "unsupported" && name === "install"
            ? "COVERAGE.FACT"
            : assertion,
        field:
          status === "unsupported" && name === "install"
            ? "coverage.observedRoles"
            : field,
      });
  }
});

test("captured-but-unselected semantic producer cannot use valid proof, including unmatched joins", async (t) => {
  for (const status of ["exact", "unmatched"]) {
    let admitted;
    const [row] = registerControls([
      {
        id: `U3.unselected ${status}`,
        baseline: () => ({ selected: true }),
        mutate: (state) => ({ ...state, selected: false }),
        check: async ({ selected }) => {
          const specimenRow = await specimen(t, {
            status,
            twoProducers: true,
            selectSecond: selected,
          });
          const { loaded, records } = specimenRow;
          const captured = {
            fixture: loaded.fixture,
            sources: [...loaded.sources].map(([key, bytes]) => [
              key,
              bytes.toString("hex"),
            ]),
            captures: [...loaded.captureBytes].map(([key, bytes]) => [
              key,
              bytes.toString("hex"),
            ]),
            native: loaded.native,
            semantic: loaded.semanticBytes.map((x) => x.value),
            facts: loaded.annotations.flatMap((x) =>
              x.facts.filter((f) => f.kind !== "coverage"),
            ),
            factTuples: loaded.annotations.flatMap((x) =>
              x.facts
                .filter((f) => f.kind === "reference")
                .map((f) => ({
                  ref: f.ref,
                  kind: f.kind,
                  anchor: f.anchor,
                  provenanceId: f.record.provenanceId,
                })),
            ),
            proofs: records.provenance,
            proofTuples: records.provenance.map(
              ({
                id,
                producerId,
                document,
                revisionId,
                contentHash,
                basis,
              }) => ({
                id,
                producerId,
                document,
                revisionId,
                contentHash,
                basis,
              }),
            ),
            coverage: loaded.annotations.flatMap((x) =>
              x.facts.filter((f) => f.kind === "coverage"),
            ),
            normalizedCoverage: records.coverage,
          };
          if (selected) {
            admitted = structuredClone(captured);
            assert.deepEqual(
              captured.fixture.producers.map((x) => x.id),
              ["native", "semantic", "semantic-b"],
            );
            assert.deepEqual(
              captured.semantic.map((x) => x.producerId),
              ["semantic", "semantic-b"],
            );
            assert.deepEqual(
              captured.facts
                .filter((x) => x.kind === "reference")
                .map((x) => x.ref),
              ["fact", "fact-b"],
            );
          } else {
            for (const field of [
              "fixture",
              "sources",
              "captures",
              "native",
              "semantic",
              "facts",
              "factTuples",
              "proofs",
              "proofTuples",
            ])
              assert.equal(
                canonical(captured[field]),
                canonical(admitted[field]),
                `${status}: fixed ${field}`,
              );
            for (const field of ["coverage", "normalizedCoverage"]) {
              assert.deepEqual(
                captured[field]
                  .map((x) => (field === "coverage" ? x.ref : x.producerId))
                  .sort(),
                admitted[field]
                  .map((x) => (field === "coverage" ? x.ref : x.producerId))
                  .sort(),
              );
              for (const entry of captured[field]) {
                const current = field === "coverage" ? entry.record : entry;
                const previousEntry = admitted[field].find(
                  (x) =>
                    (field === "coverage" ? x.ref : x.producerId) ===
                    (field === "coverage" ? entry.ref : entry.producerId),
                );
                const previous =
                  field === "coverage" ? previousEntry.record : previousEntry;
                if (current.producerId === "semantic-b") {
                  const fixed = ({
                    selected: selection,
                    state,
                    diagnostic,
                    observedRoles,
                    ...rest
                  }) => rest;
                  assert.equal(
                    canonical(fixed(current)),
                    canonical(fixed(previous)),
                    `${status}: one selection claim and its policy projections`,
                  );
                  assert.deepEqual(
                    [
                      previous.selected,
                      previous.state,
                      previous.diagnostic,
                      previous.observedRoles,
                    ],
                    [true, "complete", null, ["read", "call"]],
                  );
                  assert.deepEqual(
                    [
                      current.selected,
                      current.state,
                      current.diagnostic,
                      current.observedRoles,
                    ],
                    [false, "omitted", "producer not selected", []],
                  );
                } else
                  assert.equal(
                    canonical(current),
                    canonical(previous),
                    `${status}: other coverage unchanged`,
                  );
              }
            }
          }
          assert.doesNotThrow(
            () => checkCoverage(loaded, records),
            `${status}: admitted coverage`,
          );
          assert.doesNotThrow(
            () => checkMeasurement(loaded, records),
            `${status}: admitted measurement`,
          );
          const result = check(specimenRow);
          assert.equal(result.joined.get("fact-b").join.status, status);
          assert.equal(result.joined.get("fact-b").producerId, "semantic-b");
          return result;
        },
        expectedAssertion: "COVERAGE.FACT",
        expectedCode: "invalidRecord",
        expectedField: "coverage",
      },
    ]);
    await runControl(row);
  }
});

function reportGap(s, kind, start, end) {
  const intent = s.loaded.fixture.coverageIntents.find((row) => row.producerId === "native");
  const families = structuredClone(intent.measurementSupport);
  const family = families.find((row) => row.kind === kind);
  family.gaps.push({ range: span(start, end), diagnostic: "synthetic opaque identifier construct" });
  s.loaded.native.capabilityReports = [{ document, revisionId: "r1", families }];
  intent.measurementSupport = structuredClone(families);
  const nativeCoverage = s.loaded.annotations.flatMap((row) => row.facts).find((fact) => fact.kind === "coverage" && fact.record.producerId === "native").record;
  nativeCoverage.state = "partial";
  nativeCoverage.diagnostic = "native requested family has a construct gap";
  return families;
}

function independentlyCheckGaps(s) {
  const records = normalizeFixture(s.loaded).records;
  const coverage = checkCoverage(s.loaded, records);
  const measurement = checkMeasurement(s.loaded, records);
  return { records, joins: checkJoins(s.loaded, records, coverage, measurement) };
}

test("range gaps contain whole anchors, preserve boundary-crossing/outside and enclosing measurements", async (t) => {
  for (const [name, start, end, expected] of [
    ["contained", 0, 8, "unsupported"],
    ["crossing", 1, 8, "unmatched"],
    ["touching", 8, 9, "unmatched"],
    ["outside", 10, 12, "unmatched"],
  ]) await t.test(name, async (t) => {
    const s = await specimen(t, { status: "unmatched" });
    reportGap(s, "reference", start, end);
    const { records, joins } = independentlyCheckGaps(s);
    const join = joins.joined.get("fact").join;
    assert.equal(join.status, expected);
    assert.deepEqual(join.candidateIds, []);
    assert.equal(join.diagnostic, expected === "unsupported" ? "synthetic opaque identifier construct" : "unmatched");
    assert.equal(records.references.length, 0);
    const nativeCoverage = records.coverage.find((row) => row.producerId === "native");
    assert.equal(nativeCoverage.state, "partial");
    assert.deepEqual(nativeCoverage.observedRoles, ["read", "call"]);
    assert.equal(records.coverage.find((row) => row.producerId === "semantic").state, "complete");
  });
  for (const family of ["reference", "invocation"]) await t.test(`enclosing or outside ${family}`, async (t) => {
    const s = await specimen(t, { family });
    reportGap(s, family, family === "invocation" ? 20 : 0, family === "invocation" ? 22 : 8);
    const { joins } = independentlyCheckGaps(s);
    assert.equal(joins.joined.get("fact").join.status, "exact");
  });
});

test("captured capability report mutations fail independently before a join can hide candidates", async (t) => {
  for (const [name, mutate, field, assertion = "JOIN.SUPPORT"] of [
    ["candidate in gap", (s) => {
      const report = s.loaded.native.capabilityReports[0];
      report.families.find((row) => row.kind === "reference").gaps[0].range = span(18, 24);
      s.loaded.fixture.coverageIntents.find((row) => row.producerId === "native").measurementSupport = structuredClone(report.families);
    }, "measurementSupport"],
    ["false authored override", (s) => { s.loaded.fixture.coverageIntents.find((row) => row.producerId === "native").measurementSupport[0].available = false; }, "measurementSupport"],
    ["wrong revision", (s) => { s.loaded.native.capabilityReports[0].revisionId = "absent"; }, "capabilityReports"],
    ["wrong document", (s) => { s.loaded.native.capabilityReports[0].document = { ...document, path: "src/absent.js" }; }, "capabilityReports"],
    ["duplicate tuple", (s) => { s.loaded.native.capabilityReports.push(structuredClone(s.loaded.native.capabilityReports[0])); }, "capabilityReports"],
    ["duplicate family", (s) => { s.loaded.native.capabilityReports[0].families[0] = structuredClone(s.loaded.native.capabilityReports[0].families[1]); }, "capabilityReports"],
    ["empty gap", (s) => {
      const report = s.loaded.native.capabilityReports[0];
      report.families.find((row) => row.kind === "reference").gaps[0].range = span(1, 1);
      s.loaded.fixture.coverageIntents.find((row) => row.producerId === "native").measurementSupport = structuredClone(report.families);
    }, "capabilityReports"],
    ["missing diagnostic", (s) => { delete s.loaded.native.capabilityReports[0].families.find((row) => row.kind === "reference").gaps[0].diagnostic; }, "NativeArtifact.capabilityReports[0].families[3].gaps[0].diagnostic", "FORMAT.SHAPE"],
  ]) await t.test(name, async (t) => {
    const s = await specimen(t, { status: "unmatched" });
    reportGap(s, "reference", 0, 8);
    const baseline = independentlyCheckGaps(s);
    mutate(s);
    const rejects = (error) => error.assertion === assertion && error.field === field;
    assert.throws(() => normalizeFixture(s.loaded), rejects);
    assert.throws(() => checkJoins(s.loaded, baseline.records, s.coverageResult, s.measurement), rejects);
  });
});
