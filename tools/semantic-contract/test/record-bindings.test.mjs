import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { loadFixture } from "../load.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import {
  checkMeasurement,
  orderedEnvelope,
} from "../record-check/measurement.mjs";
import { checkJoins } from "../record-check/joins.mjs";
import { checkBindings } from "../record-check/bindings.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { registerControls, runControl } from "./mutations.mjs";

const source =
  "function main() { target(); }\nfunction target() { return 1; }\nfunction other() { return 2; }\nfunction third() { return 3; }\n";
const document = {
  sourceSetId: "main",
  language: "javascript",
  path: "src/main.js",
};
const otherDocument = { ...document, path: "src/other.js" };
const otherSetDocument = {
  ...document,
  sourceSetId: "other",
  path: "src/other-set.js",
};
const targetSource = "function target() { return 4; }\n";
const sha = (value) => createHash("sha256").update(value).digest("hex");
const canon = (value) =>
  value === null || typeof value !== "object"
    ? JSON.stringify(value)
    : Array.isArray(value)
      ? `[${value.map(canon).join(",")}]`
      : `{${Object.keys(value)
          .sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)))
          .map((x) => `${canon(x)}:${canon(value[x])}`)
          .join(",")}}`;
const digest = (kind, value) =>
  createHash("sha256")
    .update(`trellis.${kind}.v1\0`)
    .update(canon(value))
    .digest("hex")
    .slice(0, 32);
const order = (rows) =>
  rows.sort((a, b) =>
    Buffer.compare(Buffer.from(canon(a)), Buffer.from(canon(b))),
  );
const span = (start, end) => ({ start, end, encoding: "utf8" });
const range = (start, end) => ({ start, end });
const witness = (field, start, end, text) => ({
  field,
  witness: { range: span(start, end), text },
});
const names = ["main", "target", "other", "third"];
const syntax = (name, doc = document) =>
  `sid:v1:${digest("syntax", { sourceSet: doc.sourceSetId, path: doc.path, language: "javascript", ancestors: [], declaration: { kind: "function", name, signature: null, ordinal: 0 } })}`;
// Decision 0003: the call ID binds the main document's bytes, its extraction
// context (empty declared native inventory) and the native producer, not a revision.
const callId = (text = source) =>
  createHash("sha256")
    .update("trellis.occurrence.v2\0")
    .update(
      canon({
        contentHash: sha(text),
        extractionContext: sha(
          "trellis.extraction-context.v1\0" +
            canon({ language: "javascript", components: [] }),
        ),
        nativeProducerId: "native",
        nativeProducerVersion: "1",
        ownerSyntaxId: syntax("main"),
        kind: "call",
        ordinal: 0,
      }),
    )
    .digest("hex")
    .replace(/^(.{32}).*$/, "occ:v2:$1");
const internal = (name, rev = "r1", doc = document) => ({
  kind: "internal",
  syntaxId: syntax(name, doc),
  document: doc,
  revisionId: rev,
});
const external = {
  kind: "external",
  symbol: {
    scheme: "scip",
    symbol: "pkg external",
    scope: "global",
    document: null,
  },
};
const header = (name) => ({
  kind: "function",
  name,
  modifiers: [],
  typeParameters: [],
  parameters: [],
  resultType: null,
  bases: [],
});
const nativeProducer = {
  id: "native",
  version: "1",
  executableHash: sha("native executable"),
  kind: "native",
  languages: ["javascript"],
  positionEncoding: "utf8",
};
const semanticProducer = {
  id: "semantic",
  version: "1",
  executableHash: sha("semantic executable"),
  kind: "semantic",
  languages: ["javascript"],
  positionEncoding: "utf8",
};
function declarations(text, rev = "r1", doc = document, entries = names) {
  return entries.map((name) => {
    const start = text.indexOf(`function ${name}()`),
      end = text.indexOf("}", start) + 1,
      begin = start + 9;
    return {
      ref:
        doc === document
          ? `${rev}:${name}`
          : `${rev}:${doc.sourceSetId}:${doc.path}:${name}`,
      nativeId: null,
    role: "production",
      document: doc,
      revisionId: rev,
      parentRef: null,
      kind: "function",
      name,
      range: span(start, end),
      nameRange: span(begin, begin + name.length),
      header: header(name),
      signature: null,
      witnesses: [
        witness("name", begin, begin + name.length, name),
        witness("header.name", begin, begin + name.length, name),
      ],
    };
  });
}
function call(text, rev = "r1") {
  const start = text.indexOf("target();");
  return {
    ref: `${rev}:call`,
    nativeId: null,
    document,
    revisionId: rev,
    ownerRef: `${rev}:main`,
    range: span(start, start + 8),
    calleeRange: span(start, start + 6),
    spelling: "target",
    regionRefs: [],
    witnesses: [witness("spelling", start, start + 6, "target")],
  };
}
const empty = () => ({
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
// Expected IDs, target identities and rows are authored from source bytes, not from any checker output.
async function specimen(
  t,
  {
    claims = ["target"],
    dispatch = "direct",
    resolution = "resolved",
    status = "exact",
    history = false,
    changedSource = false,
    producers = false,
    possibleDispatch = [],
    externalSymbol = external.symbol,
    raw = null,
    rawAfterProof = null,
    output = null,
    captureMissing = false,
    reassignProducer = false,
    bothRevisions = false,
    admittedTargets = false,
  } = {},
) {
  const root = await mkdtemp(join(tmpdir(), "binding-u5-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const files = new Map(),
    put = (path, value) =>
      files.set(
        path,
        typeof value === "string" ? value : JSON.stringify(value),
      );
  const r2text = changedSource
    ? source.replace(
        "function target() { return 1; }",
        "function target() { return 9; }",
      )
    : source;
  const revisions =
      history || bothRevisions || admittedTargets ? ["r1", "r2"] : ["r1"],
    texts = { r1: source, r2: r2text };
  const documents = admittedTargets
    ? [document, otherDocument, otherSetDocument]
    : [document];
  const textFor = (doc, rev) => (doc === document ? texts[rev] : targetSource);
  const docsIn = (setId) =>
    documents.filter((doc) => doc.sourceSetId === setId);
  const fileFor = (doc, rev) => `snapshots/${rev}/${doc.path}`;
  const producersList = producers
    ? [
        nativeProducer,
        semanticProducer,
        {
          ...semanticProducer,
          id: "other-semantic",
          executableHash: sha("other semantic executable"),
        },
      ]
    : [nativeProducer, semanticProducer];
  const allDeclarations = revisions.flatMap((rev) =>
      documents.flatMap((doc) =>
        declarations(
          textFor(doc, rev),
          rev,
          doc,
          doc === document ? names : ["target"],
        ),
      ),
    ),
    allCalls = revisions.map((rev) => call(texts[rev], rev));
  put("captures/native.json", {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    declarations: allDeclarations,
    calls: allCalls,
    controls: [],
    references: [],
  });
  for (const [name, value] of [
    ["native", "native executable"],
    ["semantic", "semantic executable"],
    ...(producers ? [["other-semantic", "other semantic executable"]] : []),
    ["toolchain", "toolchain"],
    ["config", "config"],
    ["dependency", "dependency"],
  ])
    put(`captures/${name}.txt`, value);
  const nativeCaptures = [
    ["native", "executable", "captures/native.txt"],
    ["semantic", "executable", "captures/semantic.txt"],
    ...(producers
      ? [["other-semantic", "executable", "captures/other-semantic.txt"]]
      : []),
    ["toolchain", "toolchain", "captures/toolchain.txt"],
    ["config", "config", "captures/config.txt"],
    ["dependency", "dependency", "captures/dependency.txt"],
  ].map(([ref, kind, file]) => ({
    ref,
    kind,
    file,
    hash: sha(files.get(file)),
  }));
  const artifactPaths = producers
    ? ["captures/semantic.json", "captures/other-semantic.json"]
    : ["captures/semantic.json"];
  const authoredClaims = bothRevisions
    ? revisions.flatMap((rev) => claims.map((name, i) => ({ name, rev, i })))
    : claims.map((name, i) => ({ name, rev: "r1", i }));
  const facts = authoredClaims.map(({ name, rev, i }, index) => {
    const producerId =
      producers && i === claims.length - 1 ? "other-semantic" : "semantic";
    const text = texts[rev],
      begin = call(text, rev).range.start;
    const anchorRange =
      status === "exact"
        ? span(begin, begin + 6)
        : span(
            text.indexOf("return") >= 0
              ? text.indexOf("return")
              : text.indexOf("main"),
            text.indexOf("return") >= 0
              ? text.indexOf("return") + 6
              : text.indexOf("main") + 4,
          );
    return {
      producerId,
      fact: {
        kind: "callBinding",
        ref: `binding-${index}`,
        anchor: {
          document,
          revisionId: rev,
          contentHash: sha(text),
          kind: "callee",
          range: anchorRange,
          ownerRef: `${rev}:main`,
        },
        record: {
          resolution,
          declaredTarget:
            resolution === "resolved"
              ? {
                  kind: "internal",
                  declarationRef: `${rev}:${name}`,
                  revisionId: rev,
                }
              : resolution === "external"
                ? { kind: "external", symbol: externalSymbol }
                : null,
          candidates:
            resolution === "ambiguous"
              ? ["target", "other"]
                  .sort((a, b) =>
                    Buffer.compare(
                      Buffer.from(canon(internal(a))),
                      Buffer.from(canon(internal(b))),
                    ),
                  )
                  .map((name) => ({
                    kind: "internal",
                    declarationRef: `r1:${name}`,
                    revisionId: "r1",
                  }))
              : [],
          dispatch,
          possibleDispatch: possibleDispatch.map((name) => ({
            kind: "internal",
            declarationRef: `r1:${name}`,
            revisionId: "r1",
          })),
          possibleDispatchComplete: false,
          provenanceId: `proof-${index}`,
        },
      },
    };
  });
  if (status !== "exact")
    for (const { fact } of facts) {
      fact.anchor.range = span(0, 8);
      fact.anchor.ownerRef = "r1:main";
    }
  raw?.(facts);
  for (const id of producersList
    .filter((x) => x.kind === "semantic")
    .map((x) => x.id))
    put(`captures/${id}.json`, {
      formatVersion: 1,
      producerId: id,
      facts: facts.filter((x) => x.producerId === id).map((x) => x.fact),
    });
  const captures = [
    ...nativeCaptures,
    ...artifactPaths.map((file, i) => ({
      ref: `artifact-${i}`,
      kind: "semanticArtifact",
      file,
      hash: sha(files.get(file)),
    })),
  ];
  const cap = (ref) => captures.find((x) => x.ref === ref).hash;
  const revision = (rev, setId) => ({
    id: rev,
    sourceSetId: setId,
    documents: docsIn(setId).map((doc) => ({
      key: doc,
      revisionId: rev,
      sourceFile: fileFor(doc, rev),
    })),
    toolchainHash: cap("toolchain"),
    configHash: cap("config"),
    dependencyHash: cap("dependency"),
  });
  const basis = (producerId, rev) => ({
    producerId,
    producerVersion: "1",
    producerHash: producersList.find((x) => x.id === producerId).executableHash,
    artifactHash: captures.find((x) => x.file === `captures/${producerId}.json`)
      .hash,
    language: "javascript",
    sourceSetId: "main",
    revisionId: rev,
    sourceManifestHash: sha(
      canon(
        docsIn("main").map((doc) => ({
          document: doc,
          contentHash: sha(textFor(doc, rev)),
        })),
      ),
    ),
    toolchainHash: cap("toolchain"),
    configHash: cap("config"),
    dependencyHash: cap("dependency"),
    lookupDependencies: [],
  });
  const proofs = facts.map(({ fact, producerId }) => ({
    id: fact.record.provenanceId,
    producerId,
    document,
    revisionId: fact.anchor.revisionId,
    contentHash: sha(texts[fact.anchor.revisionId]),
    evidenceKind: "semanticReference",
    basis: basis(producerId, fact.anchor.revisionId),
    freshness:
      fact.anchor.revisionId === "r1" && revisions.length > 1
        ? changedSource
          ? "stale"
          : "possiblyStale"
        : "fresh",
  }));
  if (rawAfterProof || reassignProducer) {
    if (reassignProducer) {
      facts[0].producerId = "other-semantic";
      proofs[0].producerId = "other-semantic";
      proofs[0].basis = basis("other-semantic", proofs[0].revisionId);
    }
    rawAfterProof?.(facts);
    for (const id of producersList
      .filter((x) => x.kind === "semantic")
      .map((x) => x.id)) {
      const file = `captures/${id}.json`;
      put(file, {
        formatVersion: 1,
        producerId: id,
        facts: facts.filter((x) => x.producerId === id).map((x) => x.fact),
      });
      const artifactHash = sha(files.get(file));
      captures.find((x) => x.file === file).hash = artifactHash;
      for (const proof of proofs)
        if (proof.producerId === id) proof.basis.artifactHash = artifactHash;
    }
  }
  // Old evidence tracks both changed source bytes and unchanged bytes in a newer revision.
  const coverage = (producerId, rev, doc = document) => ({
    producerId,
    language: "javascript",
    sourceSetId: doc.sourceSetId,
    documentPath: doc.path,
    revisionId: rev,
    requested: true,
    selected: true,
    state: status === "unsupported" ? "partial" : "complete",
    supportedRoles: ["read", "call"],
    observedRoles: status === "unsupported" ? ["read"] : ["read", "call"],
    diagnostic:
      status === "unsupported" ? "call measurement unavailable" : null,
  });
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
      ...(admittedTargets
        ? [
            {
              id: "other",
              rootId: "other-root",
              languages: ["javascript"],
              dependencies: [],
            },
          ]
        : []),
    ],
    producers: producersList,
    revisions: revisions.flatMap((rev) => [
      revision(rev, "main"),
      ...(admittedTargets ? [revision(rev, "other")] : []),
    ]),
    comparison: {
      sourceSetId: "main",
      revisionId: revisions.at(-1),
      producers: producersList,
    },
    coverageIntents: revisions.flatMap((rev) =>
      documents.flatMap((doc) =>
        producersList.map((producer) => ({
          producerId: producer.id,
          document: doc,
          revisionId: rev,
          requestedRoles: ["read", "call"],
          measurementSupport: [
            "declarationName",
            "callee",
            "invocation",
            "reference",
          ].map((kind) => ({
            kind,
            available: status !== "unsupported" || kind !== "callee",
            diagnostic:
              status === "unsupported" && kind === "callee"
                ? "unsupported"
                : null,
          })),
        })),
      ),
    ),
    nativeArtifact: "captures/native.json",
    semanticArtifacts: artifactPaths,
    annotationFiles: revisions.flatMap((rev) =>
      documents.map((doc) => `${fileFor(doc, rev)}.annotations.json`),
    ),
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  for (const rev of revisions)
    for (const doc of documents) {
      put(fileFor(doc, rev), textFor(doc, rev));
      put(`${fileFor(doc, rev)}.annotations.json`, {
        formatVersion: 1,
        document: doc,
        revisionId: rev,
        scenarios: [],
        facts: [
          ...producersList.map((p) => ({
            kind: "coverage",
            ref: `coverage-${p.id}-${rev}-${doc.path}`,
            record: coverage(p.id, rev, doc),
          })),
          ...(doc === document
            ? proofs
                .filter((x) => x.revisionId === rev)
                .map((record, i) => ({
                  kind: "provenance",
                  ref: `provenance-${rev}-${i}`,
                  record,
                }))
            : []),
          ...(doc === document
            ? facts
                .filter((x) => x.fact.anchor.revisionId === rev)
                .map((x) => x.fact)
            : []),
        ],
      });
    }
  if (captureMissing) {
    const capture = JSON.parse(files.get("captures/semantic.json"));
    capture.facts.shift();
    put("captures/semantic.json", capture);
    const artifactHash = sha(files.get("captures/semantic.json"));
    captures.find((x) => x.file === "captures/semantic.json").hash =
      artifactHash;
    for (const proof of proofs)
      if (proof.producerId === "semantic")
        proof.basis.artifactHash = artifactHash;
    const annotation = JSON.parse(
      files.get(`${fileFor(document, "r1")}.annotations.json`),
    );
    for (const fact of annotation.facts)
      if (fact.kind === "provenance" && fact.record.producerId === "semantic")
        fact.record.basis.artifactHash = artifactHash;
    put(`${fileFor(document, "r1")}.annotations.json`, annotation);
  }
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  for (const [path, value] of files) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await writeFile(join(root, path), value);
  }
  const loaded = await loadFixture(root),
    records = empty();
  records.comparison = fixture.comparison;
  records.producers = order(structuredClone(producersList));
  records.sourceSets = structuredClone(fixture.sourceSets);
  records.revisions = fixture.revisions.map((row) => ({
    ...row,
    documents: row.documents.map((item) => ({
      key: item.key,
      revisionId: row.id,
      contentHash: sha(textFor(item.key, row.id)),
      byteLength: Buffer.byteLength(textFor(item.key, row.id)),
    })),
  }));
  records.revisions = order(records.revisions);
  records.coverage = order(
    revisions.flatMap((rev) =>
      documents.flatMap((doc) =>
        producersList.map((p) => coverage(p.id, rev, doc)),
      ),
    ),
  );
  records.declarations = allDeclarations
    .map((d) => ({
      syntaxId: syntax(d.name, d.document),
      role: "production",
      document: d.document,
      revisionId: d.revisionId,
      kind: "function",
      name: d.name,
      lookupKey: d.name,
      ancestors: [],
      key: { kind: "function", name: d.name, signature: null, ordinal: 0 },
      range: range(d.range.start, d.range.end),
      nameRange: range(d.nameRange.start, d.nameRange.end),
      header: d.header,
      provenanceId: `native:${d.revisionId}:${syntax(d.name, d.document)}`,
    }))
    .sort(
      (a, b) =>
        Buffer.compare(Buffer.from(a.syntaxId), Buffer.from(b.syntaxId)) ||
        Buffer.compare(Buffer.from(a.revisionId), Buffer.from(b.revisionId)) ||
        Buffer.compare(
          Buffer.from(canon(a.document)),
          Buffer.from(canon(b.document)),
        ),
    );
  records.calls = allCalls
    .map((d) => ({
      id: callId(texts[d.revisionId]),
      ownerSyntaxId: syntax("main"),
      ordinal: 0,
      document,
      revisionId: d.revisionId,
      range: range(d.range.start, d.range.end),
      calleeRange: range(d.calleeRange.start, d.calleeRange.end),
      spelling: "target",
      regionIds: [],
      provenanceId: `native:${d.revisionId}:${callId(texts[d.revisionId])}`,
    }))
    .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)));
  records.provenance = [
    ...proofs,
    ...[...allDeclarations, ...allCalls].map((d) => ({
      id: `native:${d.revisionId}:${d.kind === "function" ? syntax(d.name, d.document) : callId(texts[d.revisionId])}`,
      producerId: "native",
      document: d.document,
      revisionId: d.revisionId,
      contentHash: sha(textFor(d.document, d.revisionId)),
      evidenceKind: "measuredSyntax",
      basis: null,
      freshness:
        d.document.sourceSetId !== "main"
          ? "stale"
          : d.revisionId === "r1" && revisions.length > 1
            ? changedSource && d.document === document
              ? "stale"
              : "possiblyStale"
            : "fresh",
    })),
  ];
  const prechecks = () => {
    const C = checkCoverage(loaded, records),
      M = checkMeasurement(loaded, records),
      J = checkJoins(loaded, records, C, M);
    return { C, M, J };
  };
  const expectedJoin = (fact) => ({
    anchor: {
      document,
      revisionId: fact.anchor.revisionId,
      contentHash: sha(texts[fact.anchor.revisionId]),
      range: range(fact.anchor.range.start, fact.anchor.range.end),
      kind: "callee",
    },
    status,
    candidateIds: status === "exact" ? [callId(texts[fact.anchor.revisionId])] : [],
    diagnostic: status === "exact" ? null : status,
  });
  const contradiction =
    resolution === "resolved" &&
    status === "exact" &&
    new Set(claims).size > 1 &&
    !producers;
  records.callBindings = facts.map(({ fact }, i) => {
    const join = expectedJoin(fact),
      proof = proofs[i],
      rev = fact.anchor.revisionId,
      declaredTarget =
        resolution === "resolved"
          ? internal(claims[i % claims.length], rev)
          : resolution === "external"
            ? { kind: "external", symbol: externalSymbol }
            : null;
    const stale =
      declaredTarget?.kind === "internal"
        ? false // v1: evaluated at the binding's own revision (#52)
        : null;
    return {
      callId: status === "exact" ? callId(texts[rev]) : null,
      join,
      resolution: contradiction ? "ambiguous" : resolution,
      declaredTarget: contradiction ? null : declaredTarget,
      candidates: contradiction
        ? order([...new Set(claims)].map((name) => internal(name, rev)))
        : resolution === "ambiguous"
          ? order([internal("target", rev), internal("other", rev)])
          : [],
      dispatch,
      possibleDispatch: order(
        possibleDispatch.map((name) => internal(name, rev)),
      ),
      possibleDispatchComplete: false,
      staleTarget: contradiction ? null : stale,
      provenanceId: proof.id,
    };
  });
  records.callBindings = order(records.callBindings);
  output?.(records.callBindings, records);
  return {
    loaded,
    records,
    prechecks,
    facts,
    proofs,
    claims,
    expectedJoin,
    root,
    files,
  };
}
function check(s) {
  const { C, M, J } = s.prechecks();
  return checkBindings(s.loaded, s.records, C, M, J);
}

// These expected rows come from authored source offsets, declaration identities and proof IDs.
// They do not read the binding checker result to calculate any expected member.
function expectedMembers(names, { possibleDispatch = [] } = {}) {
  const targets = order([...new Set(names)].map((name) => internal(name)));
  const contradictory = targets.length > 1;
  const start = source.indexOf("target();");
  const join = {
    anchor: {
      document,
      revisionId: "r1",
      contentHash: sha(source),
      range: range(start, start + 6),
      kind: "callee",
    },
    status: "exact",
    candidateIds: [callId()],
    diagnostic: null,
  };
  return order(
    names.map((name, i) => ({
      callId: callId(),
      join,
      resolution: contradictory ? "ambiguous" : "resolved",
      declaredTarget: contradictory ? null : internal(name),
      candidates: contradictory ? targets : [],
      dispatch: "direct",
      possibleDispatch: order(possibleDispatch.map((x) => internal(x))),
      possibleDispatchComplete: false,
      staleTarget: contradictory ? null : false,
      provenanceId: `proof-${i}`,
    })),
  );
}
test("loaded exact claims retain full member/proof closure and canonical bytes for one, two and three targets", async (t) => {
  for (const names of [
    ["target", "target"],
    ["target", "other"],
    ["target", "other", "third"],
  ]) {
    const options = {
      claims: names,
      possibleDispatch: ["other", "third"],
      history: true,
      changedSource: true,
    };
    const s = await specimen(t, options),
      B = check(s),
      members = expectedMembers(names, options);
    assert.equal(canon(B.callBindings), canon(members));
    assert.equal(canon(s.records.callBindings), canon(members));
    assert.equal(
      Buffer.compare(
        Buffer.from(canon(B.callBindings)),
        Buffer.from(canon(members)),
      ),
      0,
    );
    assert.equal(B.groups.size, 1);
    const group = [...B.groups.values()][0];
    const proofIds = names.map((_, i) => `proof-${i}`),
      factRefs = names.map((_, i) => `binding-${i}`);
    assert.equal(canon(group.members), canon(members));
    assert.deepEqual(group.factRefs, factRefs);
    assert.deepEqual(group.provenanceIds, proofIds);
    assert.equal(
      canon(group.targetProofs),
      canon(
        names.map((name, i) => ({
          factRef: `binding-${i}`,
          provenanceId: `proof-${i}`,
          declaredTarget: internal(name),
          candidates: [],
          possibleDispatch: order(["other", "third"].map((x) => internal(x))),
          proof: s.proofs[i],
        })),
      ),
    );
    assert.equal(
      canon(group.historicalTuples),
      canon(
        names.map((_, i) => ({
          factRef: `binding-${i}`,
          producerId: "semantic",
          document,
          revisionId: "r1",
          freshness: "stale",
        })),
      ),
    );
    assert.equal(
      canon([...B.recordByFactRef.entries()]),
      canon(
        names.map((_, i) => [
          `binding-${i}`,
          members.find((x) => x.provenanceId === `proof-${i}`),
        ]),
      ),
    );
    assert.deepEqual([...s.loaded.semanticProofs.keys()], proofIds);
  }
});

test("a byte-identical output duplicate is rejected; two distinct admitted proofs cannot collapse", async (t) => {
  const s = await specimen(t, { claims: ["target", "target"] }),
    B = check(s);
  assert.equal(
    canon(B.callBindings),
    canon(expectedMembers(["target", "target"])),
  );
  s.records.callBindings.splice(
    1,
    0,
    structuredClone(s.records.callBindings[0]),
  );
  assert.throws(
    () => check(s),
    (e) =>
      e.assertion === "BINDING.CONTRIBUTORS" &&
      e.code === "invalidRecord" &&
      e.field === "callBindings",
  );
  // A second raw fact with the same ref cannot represent another byte-identical
  // member: loadFixture rejects it before a binding group can be formed.
  await assert.rejects(
    specimen(t, {
      claims: ["target", "target"],
      raw: (facts) => {
        facts[1].fact.ref = facts[0].fact.ref;
      },
    }),
    (e) =>
      e.assertion === "IDENTITY.SEMANTIC" &&
      e.code === "invalidRecord" &&
      e.field === "facts",
  );
});

test("all resolution cardinalities, six dispatch classes, producer isolation and nonexact groups", async (t) => {
  for (const dispatch of [
    "direct",
    "constructor",
    "virtual",
    "interface",
    "dynamic",
    "unknown",
  ]) {
    const s = await specimen(t, { dispatch });
    assert.equal(check(s).callBindings[0].dispatch, dispatch);
  }
  for (const resolution of [
    "resolved",
    "external",
    "ambiguous",
    "unresolved",
  ]) {
    const s = await specimen(t, { resolution });
    assert.equal(check(s).callBindings[0].resolution, resolution);
  }
  const nonexact = await specimen(t, { status: "unmatched" }),
    B = check(nonexact);
  assert.equal(B.callBindings[0].callId, null);
  assert.equal([...B.groups.values()][0].anchor.kind, "callee");
  const independent = await specimen(t, {
    claims: ["target", "other"],
    producers: true,
  });
  assert.equal(check(independent).groups.size, 2);
});

test("historical proof remains linked to r1; a changed r2 target never marks the r1 binding stale", async (t) => {
  for (const changedSource of [false, true]) {
    const s = await specimen(t, { history: true, changedSource }),
      B = check(s),
      member = B.callBindings[0];
    assert.equal(member.callId, callId());
    // v1: staleTarget is evaluated at the binding's own revision (#52).
    assert.equal(member.staleTarget, false);
    assert.equal(B.groups.size, 1);
    assert.equal(
      [...B.groups.values()][0].historicalTuples[0].revisionId,
      "r1",
    );
    assert.equal(
      s.proofs[0].freshness,
      changedSource ? "stale" : "possiblyStale",
    );
  }
});

test("two measured revisions close each call, fact, proof and historical group independently", async (t) => {
  const names = ["target", "other"],
    s = await specimen(t, {
      claims: names,
      bothRevisions: true,
      changedSource: true,
    });
  const { C, M, J } = s.prechecks(),
    B = checkBindings(s.loaded, s.records, C, M, J);
  assert.deepEqual(
    s.loaded.native.calls.map((x) => x.ref),
    ["r1:call", "r2:call"],
  );
  assert.deepEqual(
    s.loaded.annotations
      .filter((a) => a.document.path === document.path)
      .map((a) => a.revisionId),
    ["r1", "r2"],
  );
  assert.deepEqual(
    [...s.loaded.semanticProofs.keys()],
    ["proof-0", "proof-1", "proof-2", "proof-3"],
  );
  assert.equal(B.groups.size, 2);
  const expected = [];
  for (const [revision, base, freshness] of [
    ["r1", 0, "stale"],
    ["r2", 2, "fresh"],
  ]) {
    const text =
      revision === "r1"
        ? source
        : source.replace(
            "function target() { return 1; }",
            "function target() { return 9; }",
          );
    const start = text.indexOf("target();"),
      targets = order(names.map((name) => internal(name, revision)));
    const join = {
      anchor: {
        document,
        revisionId: revision,
        contentHash: sha(text),
        range: range(start, start + 6),
        kind: "callee",
      },
      status: "exact",
      candidateIds: [callId(text)],
      diagnostic: null,
    };
    const members = order(
      names.map((_, i) => ({
        callId: callId(text),
        join,
        resolution: "ambiguous",
        declaredTarget: null,
        candidates: targets,
        dispatch: "direct",
        possibleDispatch: [],
        possibleDispatchComplete: false,
        staleTarget: null,
        provenanceId: `proof-${base + i}`,
      })),
    );
    expected.push(...members);
    const groups = [...B.groups.values()].filter(
      (g) =>
        g.producerId === "semantic" &&
        g.revisionId === revision &&
        g.callId === callId(text),
    );
    assert.equal(groups.length, 1);
    const group = groups[0];
    const factRefs = names.map((_, i) => `binding-${base + i}`),
      proofIds = names.map((_, i) => `proof-${base + i}`);
    assert.deepEqual(group.factRefs, factRefs);
    assert.deepEqual(group.provenanceIds, proofIds);
    assert.equal(canon(group.members), canon(members));
    assert.equal(
      canon(group.targetProofs),
      canon(
        names.map((name, i) => ({
          factRef: factRefs[i],
          provenanceId: proofIds[i],
          declaredTarget: internal(name, revision),
          candidates: [],
          possibleDispatch: [],
          proof: s.proofs[base + i],
        })),
      ),
    );
    assert.equal(
      canon(group.historicalTuples),
      canon(
        names.map((_, i) => ({
          factRef: factRefs[i],
          producerId: "semantic",
          document,
          revisionId: revision,
          freshness,
        })),
      ),
    );
    for (let i = 0; i < names.length; i++) {
      assert.equal(
        canon(B.recordByFactRef.get(factRefs[i])),
        canon(members.find((row) => row.provenanceId === proofIds[i])),
      );
      assert.equal(s.facts[base + i].fact.anchor.revisionId, revision);
      assert.equal(s.proofs[base + i].basis.revisionId, revision);
      assert.equal(s.proofs[base + i].contentHash, sha(text));
      assert.equal(
        s.proofs[base + i].basis.sourceManifestHash,
        sha(canon([{ document, contentHash: sha(text) }])),
      );
      assert.equal(
        s.proofs[base + i].basis.artifactHash,
        sha(s.files.get("captures/semantic.json")),
      );
      assert.equal(s.facts[base + i].fact.anchor.contentHash, sha(text));
    }
    assert.equal(s.proofs[base].freshness, freshness);
  }
  assert.equal(canon(B.callBindings), canon(order(expected)));
  assert.equal(canon(s.records.callBindings), canon(order(expected)));
  const wrong = await specimen(t, {
    claims: names,
    bothRevisions: true,
    changedSource: true,
    output: (rows) => {
      rows.find((row) => row.provenanceId === "proof-0").callId = callId("r2");
      rows.splice(0, rows.length, ...orderedEnvelope("callBindings", rows));
    },
  });
  const checks = wrong.prechecks();
  assert.throws(
    () =>
      checkBindings(wrong.loaded, wrong.records, checks.C, checks.M, checks.J),
    (e) =>
      e.assertion === "BINDING.JOIN" &&
      e.code === "invalidRecord" &&
      e.field === "join",
  );
});

test("admitted target tuples in two documents, source sets and revisions cannot replace the claim", async (t) => {
  const baseline = await specimen(t, { admittedTargets: true });
  assert.equal(
    check(baseline).callBindings[0].declaredTarget.syntaxId,
    syntax("target"),
  );
  for (const doc of [otherDocument, otherSetDocument])
    for (const revision of ["r1", "r2"]) {
      assert(
        baseline.loaded.native.declarations.some(
          (row) =>
            row.document.path === doc.path &&
            row.revisionId === revision &&
            row.name === "target",
        ),
      );
      assert(
        baseline.records.declarations.some(
          (row) =>
            row.syntaxId === syntax("target", doc) &&
            row.revisionId === revision,
        ),
      );
    }
  const alternatives = [
    ["document", otherDocument, "r1"],
    ["source-set", otherSetDocument, "r1"],
    ["revision", document, "r2"],
  ];
  for (const [label, doc, revision] of alternatives) {
    const wrong = await specimen(t, {
      admittedTargets: true,
      output: (rows) => {
        rows[0].declaredTarget = internal("target", revision, doc);
      },
    });
    const { C, M, J } = wrong.prechecks();
    assert.throws(
      () => checkBindings(wrong.loaded, wrong.records, C, M, J),
      (e) => {
        assert.equal(e.assertion, "BINDING.CARDINALITY", label);
        assert.equal(e.code, "invalidRecord", label);
        assert.equal(e.field, "resolution", label);
        return true;
      },
    );
  }
  const raw = await specimen(t, {
    admittedTargets: true,
    raw: (facts) => {
      facts[0].fact.record.declaredTarget = {
        kind: "internal",
        declarationRef: "r1:main:src/other.js:target",
        revisionId: "r1",
      };
    },
  });
  const { C, M, J } = raw.prechecks();
  assert.throws(
    () => checkBindings(raw.loaded, raw.records, C, M, J),
    (e) =>
      e.assertion === "BINDING.CARDINALITY" &&
      e.code === "invalidRecord" &&
      e.field === "resolution",
  );
});

test("two producers keep full separate proof inventories; reassignment merges only their raw claim group", async (t) => {
  const s = await specimen(t, { claims: ["target", "other"], producers: true }),
    B = check(s);
  assert.equal(B.groups.size, 2);
  assert.equal(
    canon(B.callBindings),
    canon(
      order(
        ["target", "other"].map((name, i) => ({
          callId: callId(),
          join: s.expectedJoin(s.facts[i].fact),
          resolution: "resolved",
          declaredTarget: internal(name),
          candidates: [],
          dispatch: "direct",
          possibleDispatch: [],
          possibleDispatchComplete: false,
          staleTarget: false,
          provenanceId: `proof-${i}`,
        })),
      ),
    ),
  );
  for (const [producer, i, name] of [
    ["semantic", 0, "target"],
    ["other-semantic", 1, "other"],
  ]) {
    const groups = [...B.groups.values()].filter(
      (g) => g.producerId === producer,
    );
    assert.equal(groups.length, 1);
    const group = groups[0],
      factRef = `binding-${i}`,
      proofId = `proof-${i}`;
    const member = {
      callId: callId(),
      join: s.expectedJoin(s.facts[i].fact),
      resolution: "resolved",
      declaredTarget: internal(name),
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      staleTarget: false,
      provenanceId: proofId,
    };
    assert.equal(
      canon(s.records.callBindings.find((row) => row.provenanceId === proofId)),
      canon(member),
    );
    assert.equal(group.revisionId, "r1");
    assert.equal(group.callId, callId());
    assert.deepEqual(group.factRefs, [factRef]);
    assert.deepEqual(group.provenanceIds, [proofId]);
    assert.equal(canon(group.members), canon([member]));
    assert.equal(
      canon(group.targetProofs),
      canon([
        {
          factRef,
          provenanceId: proofId,
          declaredTarget: internal(name),
          candidates: [],
          possibleDispatch: [],
          proof: s.proofs[i],
        },
      ]),
    );
    assert.equal(
      canon(group.historicalTuples),
      canon([
        {
          factRef,
          producerId: producer,
          document,
          revisionId: "r1",
          freshness: "fresh",
        },
      ]),
    );
    assert.equal(canon(B.recordByFactRef.get(factRef)), canon(member));
  }
  const reassigned = await specimen(t, {
    claims: ["target", "other"],
    producers: true,
    reassignProducer: true,
  });
  assert.deepEqual(
    reassigned.proofs.map((proof) => proof.id),
    ["proof-0", "proof-1"],
  );
  assert.equal(reassigned.proofs[0].producerId, "other-semantic");
  const { C, M, J } = reassigned.prechecks();
  assert.throws(
    () => checkBindings(reassigned.loaded, reassigned.records, C, M, J),
    (e) =>
      e.assertion === "BINDING.CONTRADICTION" &&
      e.code === "invalidRecord" &&
      e.field === "candidates",
  );
});

test("source-backed agreeing duplicate claims keep each provenance and agree at normalization/checker boundary", async (t) => {
  const cases = [
    { label: "resolved exact", options: { claims: ["target", "target"] } },
    {
      label: "external exact",
      options: { claims: ["target", "target"], resolution: "external" },
    },
    {
      label: "unresolved exact",
      options: { claims: ["target", "target"], resolution: "unresolved" },
    },
    {
      label: "ambiguous same candidates",
      options: { claims: ["target", "target"], resolution: "ambiguous" },
    },
    {
      label: "unmatched diagnostic only",
      options: { claims: ["target", "target"], status: "unmatched" },
    },
    {
      label: "unsupported diagnostic only",
      options: { claims: ["target", "target"], status: "unsupported" },
    },
  ];
  for (const { label, options } of cases) {
    const s = await specimen(t, options),
      compiled = normalizeFixture(s.loaded);
    const { C, M, J } = s.prechecks(),
      checked = checkBindings(s.loaded, s.records, C, M, J);
    assert.equal(
      canon(compiled.records.callBindings),
      canon(checked.callBindings),
      label,
    );
    assert.deepEqual(
      compiled.records.callBindings.map((x) => x.provenanceId).sort(),
      ["proof-0", "proof-1"],
      label,
    );
    for (const [factRef, proofId] of [
      ["binding-0", "proof-0"],
      ["binding-1", "proof-1"],
    ]) {
      assert.equal(
        compiled.recordMap.get(factRef).provenanceId,
        proofId,
        label,
      );
      assert.equal(
        canon(compiled.recordMap.get(factRef)),
        canon(checked.recordByFactRef.get(factRef)),
        label,
      );
    }
    assert.equal(
      canon(
        checkBindings(
          s.loaded,
          { ...s.records, callBindings: compiled.records.callBindings },
          C,
          M,
          J,
        ).callBindings,
      ),
      canon(compiled.records.callBindings),
      label,
    );
  }
});

test("source-backed contradictory nonexact targets and mismatched non-target claims fail both boundaries", async (t) => {
  const cases = [
    {
      label: "contradictory unmatched",
      options: {
        claims: ["target", "target"],
        status: "unmatched",
        raw: (facts) => {
          facts[1].fact.record.declaredTarget.declarationRef = "r1:other";
        },
      },
    },
    {
      label: "mismatched dispatch",
      options: {
        claims: ["target", "target"],
        raw: (facts) => {
          facts[1].fact.record.dispatch = "virtual";
        },
      },
    },
    {
      label: "mismatched possible dispatch",
      options: {
        claims: ["target", "target"],
        possibleDispatch: ["third"],
        raw: (facts) => {
          facts[1].fact.record.possibleDispatch = [];
        },
      },
    },
  ];
  for (const { label, options } of cases) {
    const s = await specimen(t, options),
      { C, M, J } = s.prechecks();
    assert.throws(
      () => normalizeFixture(s.loaded),
      (e) =>
        e.assertion === "NORMALIZE.BINDING_CONFLICT" &&
        e.code === "invalidRecord",
      label,
    );
    assert.throws(
      () => checkBindings(s.loaded, s.records, C, M, J),
      (e) =>
        e.assertion === "BINDING.CONTRADICTION" && e.code === "invalidRecord",
      label,
    );
  }
});

test("finite loaded binding controls run valid baseline before each one-property mutation", async (t) => {
  const cases = [
    {
      id: "BINDING.FACT.proof",
      rawAfterProof: (f) => {
        f[0].fact.record.provenanceId = "proof-missing";
      },
      assertion: "IDENTITY.SEMANTIC",
      field: "binding-0",
    },
    {
      id: "BINDING.FACT.missing-capture",
      options: {},
      captureMissing: true,
      assertion: "IDENTITY.SEMANTIC",
      field: "binding-0",
    },
    {
      id: "BINDING.JOIN.call",
      options: { bothRevisions: true },
      output: (r) => {
        r.find((x) => x.provenanceId === "proof-0").callId = callId("r2");
      },
      assertion: "BINDING.JOIN",
      field: "join",
    },
    {
      id: "BINDING.JOIN.nonexact-call",
      options: { status: "unmatched" },
      output: (r) => {
        r[0].callId = callId();
      },
      assertion: "BINDING.JOIN",
      field: "join",
    },
    {
      id: "BINDING.JOIN.unsupported-call",
      options: { status: "unsupported" },
      output: (r) => {
        r[0].callId = callId();
      },
      assertion: "BINDING.JOIN",
      field: "join",
    },
    {
      id: "BINDING.CONTRIBUTORS.omitted",
      options: { claims: ["target", "other"] },
      output: (r) => {
        r.pop();
      },
      assertion: "BINDING.CONTRIBUTORS",
      field: "callBindings",
    },
    {
      id: "BINDING.CONTRIBUTORS.extra",
      output: (r) => {
        r.push({ ...r[0], provenanceId: "invented" });
        r.splice(0, r.length, ...orderedEnvelope("callBindings", r));
      },
      assertion: "BINDING.CONTRIBUTORS",
      field: "callBindings",
    },
    {
      id: "BINDING.CARDINALITY.cross-producer-proof-swap",
      options: { claims: ["target", "other"], producers: true },
      output: (r) => {
        const a = r.find((x) => x.provenanceId === "proof-0"),
          b = r.find((x) => x.provenanceId === "proof-1");
        [a.provenanceId, b.provenanceId] = [b.provenanceId, a.provenanceId];
        r.splice(0, r.length, ...orderedEnvelope("callBindings", r));
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.CONTRADICTION.reassigned-producer-proof",
      options: { claims: ["target", "other"], producers: true },
      reassignProducer: true,
      assertion: "BINDING.CONTRADICTION",
      field: "candidates",
    },
    {
      id: "BINDING.JOIN.revision-proof-swap",
      options: { claims: ["target"], bothRevisions: true },
      output: (r) => {
        const a = r.find((x) => x.provenanceId === "proof-0"),
          b = r.find((x) => x.provenanceId === "proof-1");
        [a.provenanceId, b.provenanceId] = [b.provenanceId, a.provenanceId];
        r.splice(0, r.length, ...orderedEnvelope("callBindings", r));
      },
      assertion: "BINDING.JOIN",
      field: "join",
    },
    {
      id: "BINDING.CONTRADICTION.third-union",
      options: { claims: ["target", "other", "third"] },
      output: (r) => {
        r[0].candidates.pop();
      },
      assertion: "BINDING.CONTRADICTION",
      field: "candidates",
    },
    {
      id: "BINDING.CONTRADICTION.nonexact",
      options: { claims: ["target", "target"], status: "unmatched" },
      raw: (f) => {
        f[1].fact.record.declaredTarget.declarationRef = "r1:other";
      },
      assertion: "BINDING.CONTRADICTION",
      field: "declaredTarget",
    },
    {
      id: "BINDING.CONTRADICTION.dispatch",
      options: { claims: ["target", "other"] },
      raw: (f) => {
        f[1].fact.record.dispatch = "virtual";
      },
      assertion: "BINDING.CONTRADICTION",
      field: "callBindings",
    },
    {
      id: "BINDING.CONTRADICTION.possibleDispatch",
      options: { claims: ["target", "other"], possibleDispatch: ["third"] },
      raw: (f) => {
        f[1].fact.record.possibleDispatch = [];
      },
      assertion: "BINDING.CONTRADICTION",
      field: "callBindings",
    },
    {
      id: "BINDING.CARDINALITY.forged-ambiguity",
      output: (r) => {
        r[0].resolution = "ambiguous";
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.CARDINALITY.forged-target",
      options: { resolution: "ambiguous" },
      output: (r) => {
        r[0].declaredTarget = internal("target");
      },
      assertion: "BINDING.CARDINALITY",
      field: "candidates",
    },
    {
      id: "BINDING.CARDINALITY.forged-candidate",
      options: { resolution: "ambiguous" },
      output: (r) => {
        r[0].candidates[1] = internal("third");
      },
      assertion: "BINDING.CARDINALITY",
      field: "candidates",
    },
    {
      id: "BINDING.CARDINALITY.raw-resolution",
      raw: (f) => {
        f[0].fact.record.resolution = "ambiguous";
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.DISPATCH.output",
      output: (r) => {
        r[0].dispatch = "virtual";
      },
      assertion: "BINDING.DISPATCH",
      field: "dispatch",
    },
    {
      id: "BINDING.DISPATCH.raw-complete",
      raw: (f) => {
        f[0].fact.record.possibleDispatchComplete = true;
      },
      assertion: "FORMAT.SHAPE",
      field: "SemanticCapture.facts[0].record.possibleDispatchComplete",
    },
    {
      id: "BINDING.DISPATCH.output-complete",
      output: (r) => {
        r[0].possibleDispatchComplete = true;
      },
      assertion: "FORMAT.SHAPE",
      field: "NormalizedRecordsV1.callBindings[0].possibleDispatchComplete",
    },
    {
      id: "BINDING.DISPATCH.output-possible",
      options: { possibleDispatch: ["other"] },
      output: (r) => {
        r[0].possibleDispatch = [];
      },
      assertion: "BINDING.DISPATCH",
      field: "dispatch",
    },
    {
      id: "BINDING.TARGET.stale",
      output: (r) => {
        r[0].staleTarget = true;
      },
      assertion: "BINDING.TARGET",
      field: "staleTarget",
    },
    {
      id: "BINDING.TARGET.historical-attachment",
      options: { history: true, changedSource: true },
      output: (r) => {
        r[0].declaredTarget.revisionId = "r2";
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.TARGET.duplicate-possible",
      options: { possibleDispatch: ["target"] },
      raw: (f) => {
        f[0].fact.record.possibleDispatch.push(
          structuredClone(f[0].fact.record.possibleDispatch[0]),
        );
      },
      assertion: "BINDING.TARGET",
      field: "possibleDispatch",
    },
    {
      id: "BINDING.TARGET.duplicate-candidates",
      options: { resolution: "ambiguous" },
      raw: (f) => {
        f[0].fact.record.candidates[1] = structuredClone(
          f[0].fact.record.candidates[0],
        );
      },
      assertion: "BINDING.TARGET",
      field: "candidates",
    },
    {
      id: "BINDING.TARGET.wrong-revision",
      options: { history: true },
      raw: (f) => {
        f[0].fact.record.declaredTarget = {
          kind: "internal",
          declarationRef: "r2:target",
          revisionId: "r2",
        };
      },
      // v1: an internal target must share its binding's revision.
      assertion: "BINDING.TARGET",
      field: "declaredTarget",
    },
    {
      id: "BINDING.TARGET.measured-document",
      options: { admittedTargets: true },
      output: (r) => {
        r[0].declaredTarget = internal("target", "r1", otherDocument);
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.TARGET.measured-source-set",
      options: { admittedTargets: true },
      output: (r) => {
        r[0].declaredTarget = internal("target", "r1", otherSetDocument);
      },
      assertion: "BINDING.CARDINALITY",
      field: "resolution",
    },
    {
      id: "BINDING.TARGET.wrong-document",
      options: {
        admittedTargets: true,
        resolution: "external",
        externalSymbol: { ...external.symbol, scope: "document", document },
      },
      raw: (f) => {
        f[0].fact.record.declaredTarget.symbol.document = otherDocument;
      },
      assertion: "BINDING.TARGET",
      field: "declaredTarget",
    },
    {
      id: "BINDING.TARGET.wrong-source-set",
      options: {
        admittedTargets: true,
        resolution: "external",
        externalSymbol: { ...external.symbol, scope: "document", document },
      },
      raw: (f) => {
        f[0].fact.record.declaredTarget.symbol.document = otherSetDocument;
      },
      assertion: "BINDING.TARGET",
      field: "declaredTarget",
    },
  ];
  const rows = registerControls(
    cases.map((item) => ({
      id: item.id,
      baseline: () => ({ mutated: false }),
      mutate: (state) => ({ ...state, mutated: true }),
      check: async (state) => {
        const options = { ...(item.options ?? {}) };
        if (state.mutated && item.raw) options.raw = item.raw;
        if (state.mutated && item.rawAfterProof)
          options.rawAfterProof = item.rawAfterProof;
        if (state.mutated && item.captureMissing) options.captureMissing = true;
        if (state.mutated && item.reassignProducer)
          options.reassignProducer = true;
        if (state.mutated && item.output) options.output = item.output;
        const s = await specimen(t, options),
          { C, M, J } = s.prechecks();
        return checkBindings(s.loaded, s.records, C, M, J);
      },
      expectedAssertion: item.assertion,
      expectedCode: "invalidRecord",
      expectedField: item.field,
    })),
  );
  for (const row of rows) await runControl(row);
});

test("internal and external exact claims resolve to ambiguous Target[] with both proofs", async (t) => {
  const options = {
    claims: ["target", "other"],
    raw: (facts) => {
      facts[1].fact.record.resolution = "external";
      facts[1].fact.record.declaredTarget = {
        kind: "external",
        symbol: external.symbol,
      };
    },
    output: (rows) => {
      const candidates = order([internal("target"), external]);
      for (const row of rows) {
        row.candidates = candidates;
        row.resolution = "ambiguous";
        row.declaredTarget = null;
        row.staleTarget = null;
      }
      rows.splice(0, rows.length, ...order(rows));
    },
  };
  const s = await specimen(t, options),
    B = check(s);
  const expected = order(
    ["proof-0", "proof-1"].map((provenanceId) => ({
      callId: callId(),
      join: s.expectedJoin(s.facts[0].fact),
      resolution: "ambiguous",
      declaredTarget: null,
      candidates: order([internal("target"), external]),
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      staleTarget: null,
      provenanceId,
    })),
  );
  assert.equal(canon(B.callBindings), canon(expected));
  const compilation = normalizeFixture(s.loaded);
  assert.equal(canon(compilation.records.callBindings), canon(expected));
  for (const [factRef, proofId] of [
    ["binding-0", "proof-0"],
    ["binding-1", "proof-1"],
  ]) {
    assert.equal(compilation.recordMap.get(factRef).provenanceId, proofId);
    assert.equal(compilation.recordMap.get(factRef).resolution, "ambiguous");
    assert.equal(
      canon(compilation.recordMap.get(factRef).candidates),
      canon(order([internal("target"), external])),
    );
  }
  assert.deepEqual([...B.groups.values()][0].provenanceIds, [
    "proof-0",
    "proof-1",
  ]);
  assert.equal(
    canon(
      [...B.groups.values()][0].targetProofs.map(
        ({ declaredTarget }) => declaredTarget,
      ),
    ),
    canon([internal("target"), external]),
  );
  for (const row of B.callBindings) {
    assert.equal(row.resolution, "ambiguous");
    assert.equal(row.declaredTarget, null);
    assert.equal(row.candidates.length, 2);
  }
  const wrong = await specimen(t, {
    claims: ["target", "other"],
    raw: options.raw,
  });
  const checks = wrong.prechecks();
  assert.throws(
    () =>
      checkBindings(wrong.loaded, wrong.records, checks.C, checks.M, checks.J),
    (e) =>
      e.assertion === "BINDING.CONTRADICTION" &&
      e.code === "invalidRecord" &&
      e.field === "candidates",
  );
});

test("admitted target claims reject wrong measured refs and inconsistent contributors", async (t) => {
  for (const [label, option, assertion, field] of [
    [
      "missing-target",
      {
        raw: (facts) => {
          facts[0].fact.record.declaredTarget.declarationRef = "r1:missing";
        },
      },
      "BINDING.TARGET",
      "declaredTarget",
    ],
    [
      "revision-target",
      {
        raw: (facts) => {
          facts[0].fact.record.declaredTarget.revisionId = "r2";
        },
      },
      "BINDING.TARGET",
      "declaredTarget",
    ],
    [
      "non-target-dispatch",
      {
        claims: ["target", "other"],
        raw: (facts) => {
          facts[1].fact.record.possibleDispatch = [
            { kind: "internal", declarationRef: "r1:third", revisionId: "r1" },
          ];
        },
      },
      "BINDING.CONTRADICTION",
      "callBindings",
    ],
  ]) {
    const s = await specimen(t, option),
      { C, M, J } = s.prechecks();
    assert.throws(
      () => checkBindings(s.loaded, s.records, C, M, J),
      (error) => {
        assert.equal(error.assertion, assertion, label);
        assert.equal(error.code, "invalidRecord", label);
        assert.equal(error.field, field, label);
        return true;
      },
    );
  }
});
