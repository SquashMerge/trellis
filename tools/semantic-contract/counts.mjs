import { validate } from "./formats.mjs";
import { canonicalBytes } from "./json.mjs";
import { toByteRange } from "./coordinates.mjs";
import { checkMeasurement } from "./record-check/measurement.mjs";
import { checkCoverage } from "./record-check/coverage.mjs";
import { checkJoins } from "./record-check/joins.mjs";
import { checkRelationships } from "./record-check/relationships.mjs";
import { checkBindings } from "./record-check/bindings.mjs";

const categories = [
  "sameNameOverload",
  "importsAliases",
  "callableValues",
  "recursion",
  "relationshipsDispatch",
  "unicodeCoordinates",
  "coverageFreshness",
  "compatibilityControl",
];
const outcomes = [
  "resolved",
  "provenExternal",
  "ambiguous",
  "unresolved",
  "unsupported",
];
const roles = [
  "definition",
  "read",
  "write",
  "call",
  "type",
  "import",
  "alias",
];
const identity = (value) => canonicalBytes(value).toString("hex");
const same = (a, b) => identity(a) === identity(b);
function fail(assertion, field, message) {
  const error = new Error(`${assertion} ${field}: ${message}`);
  Object.assign(error, { assertion, field, code: "invalidRecord" });
  throw error;
}
function unique(rows, selector, field) {
  const seen = new Set();
  for (const row of rows) {
    const key = selector(row);
    if (seen.has(key))
      fail("COUNT.DUPLICATE", field, "repeated authored evidence");
    seen.add(key);
  }
}
const tuple = (row) =>
  JSON.stringify([row.document.sourceSetId, row.revisionId, row.document.path]);
const documentOf = (loaded, row) =>
  loaded.revisions
    .get(JSON.stringify([row.document.sourceSetId, row.revisionId]))
    ?.documents.find((x) => same(x.key, row.document));
function span(loaded, row, range = row.range) {
  const bytes = loaded.sources.get(tuple(row));
  if (!bytes) fail("COUNT.SOURCE", "document", "source bytes unavailable");
  return toByteRange(bytes, range);
}
function proofFor(loaded, records, fact, annotation) {
  const id =
    fact.kind === "typeRelationship"
      ? fact.provenanceRef
      : fact.record?.provenanceId;
  const proof = records.provenance.find((x) => x.id === id),
    captured = loaded.semanticProofs.get(id);
  const strip = ({ freshness, ...rest }) => rest;
  if (
    !proof ||
    !captured ||
    captured.factRef !== fact.ref ||
    captured.factKind !== fact.kind ||
    !same(strip(proof), strip(captured.wrapper)) ||
    !same(proof.document, annotation.document) ||
    proof.revisionId !== annotation.revisionId ||
    proof.contentHash !== documentOf(loaded, annotation)?.contentHash ||
    proof.evidenceKind !==
      {
        typeRelationship: "typeRelationship",
        declarationBinding: "declarationBinding",
        reference: "semanticReference",
        callBinding: "semanticReference",
      }[fact.kind]
  )
    fail(
      "COUNT.PROOF",
      "provenanceId",
      `fact is not authenticated by the captured semantic artifact and source tuple${documentOf(loaded, annotation)?.contentHash !== proof?.contentHash ? `: expected contentHash ${documentOf(loaded, annotation)?.contentHash}, actual ${proof?.contentHash}` : ""}`,
    );
  return proof;
}
function target(loaded, measurement, ref) {
  if (ref === null) return null;
  if (ref.kind === "external") return { kind: "external", symbol: ref.symbol };
  const row = measurement.recordByNativeRef.get(ref.declarationRef);
  if (!row?.syntaxId || row.revisionId !== ref.revisionId)
    fail(
      "COUNT.RELATIONSHIP",
      "typeRelationships",
      "target is not an independently measured declaration",
    );
  return {
    kind: "internal",
    syntaxId: row.syntaxId,
    document: row.document,
    revisionId: row.revisionId,
  };
}
function anchored(loaded, measurement, anchor) {
  const document = documentOf(loaded, anchor);
  const range = span(loaded, anchor);
  if (!document || anchor.contentHash !== document.contentHash)
    fail(
      "COUNT.SCENARIO",
      "anchors",
      `anchor differs from source bytes: expected ${document?.contentHash ?? "a captured document"}, actual ${anchor.contentHash}`,
    );
  return measurement.candidateRows.filter(
    (x) =>
      same(x.anchor.document, anchor.document) &&
      x.anchor.revisionId === anchor.revisionId &&
      x.anchor.kind === anchor.kind &&
      x.anchor.contentHash === document.contentHash &&
      same(x.anchor.range, range) &&
      x.ownerRef === anchor.ownerRef,
  );
}
// Re-measure native identities and inspect captured semantic wrappers. Authored
// count assertions cannot create source occurrences or turn diagnostic joins into them.
export function checkCounts(
  loaded,
  records,
  dispositions = loaded.dispositions,
) {
  validate("NormalizedRecordsV1", records);
  validate("DispositionsV1", dispositions);
  let measurement;
  try {
    measurement = checkMeasurement(loaded, records);
  } catch (error) {
    if (
      error.assertion === "RECORDS.MEMBERSHIP" &&
      ["calls", "declarations", "controlRegions"].includes(error.field)
    )
      fail(
        "COUNT.CALLS",
        "measuredCalls",
        "native occurrence IDs, owner, span or proof differ",
      );
    throw error;
  }
  const coverage = checkCoverage(loaded, records);
  const joins = checkJoins(loaded, records, coverage, measurement);
  checkRelationships(loaded, records, coverage, measurement, joins);
  checkBindings(loaded, records, coverage, measurement, joins);
  const { fixture, native, annotations } = loaded;
  const facts = new Map();
  for (const annotation of annotations)
    for (const fact of annotation.facts) {
      if (facts.has(fact.ref))
        fail("COUNT.DUPLICATE", "facts", "duplicate captured fact reference");
      facts.set(fact.ref, { fact, annotation });
    }
  const scenarios = annotations.flatMap((annotation) =>
    annotation.scenarios.map((scenario) => ({ scenario, annotation })),
  );
  unique(scenarios, (x) => x.scenario.id, "scenarios");
  const scenariosById = new Map(scenarios.map((x) => [x.scenario.id, x]));
  const usedAnchors = new Set();
  for (const { scenario, annotation } of scenarios) {
    if (!scenario.anchors.length || !scenario.factRefs.length)
      fail(
        "COUNT.SCENARIO",
        "anchors",
        "scenario requires measured anchors and connected facts",
      );
    unique(scenario.anchors, identity, "anchors");
    unique(scenario.factRefs, (x) => x, "factRefs");
    const witnessed = [];
    for (const anchor of scenario.anchors) {
      if (
        !same(anchor.document, annotation.document) ||
        anchor.revisionId !== annotation.revisionId ||
        anchored(loaded, measurement, anchor).length !== 1
      )
        fail(
          "COUNT.SCENARIO",
          "anchors",
          "scenario anchor is not uniquely measured in its document",
        );
      const key = identity([
        anchor.document,
        anchor.revisionId,
        anchor.kind,
        span(loaded, anchor),
        anchor.ownerRef,
      ]);
      if (usedAnchors.has(key))
        fail("COUNT.SCENARIO", "anchors", "reused or relabelled source anchor");
      usedAnchors.add(key);
      witnessed.push(key);
    }
    const attached = [];
    for (const ref of scenario.factRefs) {
      const entry = facts.get(ref);
      if (
        !entry ||
        !same(entry.annotation.document, annotation.document) ||
        entry.annotation.revisionId !== annotation.revisionId
      )
        fail(
          "COUNT.SCENARIO",
          "factRefs",
          "fact belongs to another source tuple",
        );
      const { fact } = entry;
      let selector = fact.anchor;
      if (fact.kind === "typeRelationship") {
        const source = native.declarations.find(
          (row) =>
            row.ref === fact.source.declarationRef &&
            row.revisionId === fact.source.revisionId,
        );
        const document = documentOf(loaded, source ?? annotation);
        if (
          !source?.nameRange ||
          !same(source.document, annotation.document) ||
          source.revisionId !== annotation.revisionId ||
          !document
        )
          fail(
            "COUNT.SCENARIO",
            "factRefs",
            "directed relationship lacks measured source",
          );
        selector = {
          document: source.document,
          revisionId: source.revisionId,
          kind: "declarationName",
          contentHash: document.contentHash,
          range: source.nameRange,
          ownerRef: source.parentRef ?? source.ref,
        };
      }
      if (
        !selector ||
        !witnessed.includes(
          identity([
            selector.document,
            selector.revisionId,
            selector.kind,
            span(loaded, selector),
            selector.ownerRef,
          ]),
        )
      )
        fail(
          "COUNT.SCENARIO",
          "factRefs",
          "fact is not joined to a scenario source anchor and tuple",
        );
      const proof = proofFor(loaded, records, fact, annotation);
      attached.push({ fact, proof });
    }
    const anchors = scenario.anchors.map(
      (anchor) => anchored(loaded, measurement, anchor)[0],
    );
    const kinds = attached.map((x) => x.fact.kind);
    const source = loaded.sources.get(tuple(annotation));
    const siblings = anchors.map((x) =>
      native.declarations.find((row) => row.ref === x.ref),
    );
    const sameNamedSiblings =
      anchors.length >= 2 &&
      anchors.every((x) => x.anchor.kind === "declarationName") &&
      siblings.every(
        (row) =>
          row &&
          row.name === siblings[0].name &&
          row.kind === siblings[0].kind &&
          row.parentRef === siblings[0].parentRef,
      );
    const includes = (role) =>
      attached.some(
        ({ fact }) =>
          fact.kind === "reference" && fact.record.roles.includes(role),
      );
    const recursive = attached.some(
      ({ fact }) =>
        fact.kind === "callBinding" &&
        fact.record.declaredTarget?.kind === "internal" &&
        fact.record.declaredTarget.declarationRef === fact.anchor.ownerRef,
    );
    const directed = attached.some(
      ({ fact }) => fact.kind === "typeRelationship",
    );
    const dispatch = attached.some(
      ({ fact }) =>
        fact.kind === "callBinding" && fact.record.dispatch !== "unknown",
    );
    const nonAscii = scenario.anchors.some((anchor) => {
      const at = span(loaded, anchor);
      return [...source.subarray(0, at.end)].some((byte) => byte >= 128);
    });
    const incomplete = attached.some(({ proof }) =>
      records.coverage.some(
        (row) =>
          row.producerId === proof.producerId &&
          row.sourceSetId === annotation.document.sourceSetId &&
          row.documentPath === annotation.document.path &&
          row.revisionId === annotation.revisionId &&
          ["partial", "failed", "omitted"].includes(row.state),
      ),
    );
    const supported = {
      sameNameOverload: sameNamedSiblings,
      importsAliases: includes("import") || includes("alias"),
      callableValues:
        includes("read") && anchors.some((x) => x.anchor.kind === "reference"),
      recursion: recursive,
      relationshipsDispatch: directed || dispatch,
      unicodeCoordinates: nonAscii,
      coverageFreshness:
        incomplete || attached.some((x) => x.proof.freshness !== "fresh"),
      compatibilityControl: includes("read") || includes("definition"),
    };
    if (!supported[scenario.category])
      fail(
        "COUNT.SCENARIO",
        "category",
        "category lacks independently measured evidence",
      );
  }
  const exactReferences = new Map(),
    referenceOccurrences = new Set();
  for (const { fact, annotation } of facts.values())
    if (fact.kind === "reference") {
      const proof = proofFor(loaded, records, fact, annotation);
      const matches = anchored(loaded, measurement, fact.anchor);
      const record = records.references.find(
        (r) => r.provenanceId === proof.id,
      );
      if (matches.length !== 1) {
        if (record)
          fail(
            "COUNT.REFERENCE",
            "references",
            "non-exact reference installed",
          );
        continue;
      }
      const candidate = matches[0],
        nativeRow = native.references.find((x) => x.ref === candidate.ref);
      if (
        !record ||
        !nativeRow ||
        record.id !== candidate.id ||
        record.ownerSyntaxId !==
          measurement.identityByRef.get(nativeRow.ownerRef) ||
        !same(record.document, nativeRow.document) ||
        record.revisionId !== nativeRow.revisionId ||
        !same(record.range, span(loaded, nativeRow)) ||
        record.provenanceId !== proof.id
      )
        fail(
          "COUNT.REFERENCE",
          "references",
          "installed reference does not match exact measured occurrence and proof",
        );
      exactReferences.set(fact.ref, { row: nativeRow, record, candidate });
      referenceOccurrences.add(
        identity([record.document, record.revisionId, record.id]),
      );
    }
  for (const record of records.references)
    if (![...exactReferences.values()].some((x) => same(x.record, record)))
      fail("COUNT.REFERENCE", "references", "unproven installed reference");
  const negativeOccurrences = new Set();
  unique(
    dispositions.callableValueNegatives,
    (x) => identity([x.scenarioId, x.referenceRef]),
    "callableValueNegatives",
  );
  for (const negative of dispositions.callableValueNegatives) {
    const scenario = scenariosById.get(negative.scenarioId)?.scenario;
    const installed = exactReferences.get(negative.referenceRef),
      row = installed?.row,
      record = installed?.record;
    const callCallees =
      row &&
      native.calls
        .filter(
          (c) =>
            same(c.document, row.document) &&
            c.revisionId === row.revisionId &&
            c.ownerRef === row.ownerRef &&
            c.calleeRange !== null,
        )
        .map((c) => span(loaded, c, c.calleeRange));
    const position = row && span(loaded, row);
    const inCallee = callCallees?.some(
      (c) => c.start < position.end && position.start < c.end,
    );
    if (
      !row ||
      !scenario ||
      scenario.category !== "callableValues" ||
      !scenario.factRefs.includes(negative.referenceRef) ||
      row.ownerRef !== negative.ownerRef ||
      !same(position, span(loaded, row, negative.range)) ||
      inCallee ||
      !record.roles.includes("read") ||
      record.roles.includes("call") ||
      !records.declarations.some(
        (d) =>
          record.declaredTarget?.kind === "internal" &&
          d.syntaxId === record.declaredTarget.syntaxId &&
          same(d.document, record.declaredTarget.document) &&
          d.revisionId === record.declaredTarget.revisionId &&
          ["function", "method", "constructor", "anonymousFunction"].includes(
            d.kind,
          ),
      )
    )
      fail(
        "COUNT.NEGATIVE",
        "callableValueNegatives",
        "callable read requires source-backed target outside every measured call callee",
      );
    negativeOccurrences.add(
      identity([row.document, row.revisionId, installed.candidate.id]),
    );
  }
  const relationships = new Set();
  for (const { fact, annotation } of facts.values())
    if (fact.kind === "typeRelationship") {
      const proof = proofFor(loaded, records, fact, annotation);
      const source = target(loaded, measurement, fact.source),
        destination = target(loaded, measurement, fact.target);
      const record = records.typeRelationships.find(
        (x) => x.provenanceId === proof.id,
      );
      if (
        source?.kind !== "internal" ||
        !same(source.document, annotation.document) ||
        source.revisionId !== annotation.revisionId ||
        !record ||
        record.kind !== fact.relationshipKind ||
        !same(record.source, source) ||
        !same(record.target, destination)
      )
        fail(
          "COUNT.RELATIONSHIP",
          "typeRelationships",
          "captured directed source, target or proof disagrees",
        );
      relationships.add(identity([record.kind, record.source, record.target]));
    }
  for (const record of records.typeRelationships)
    if (
      ![...facts.values()].some(
        ({ fact }) =>
          fact.kind === "typeRelationship" &&
          fact.provenanceRef === record.provenanceId,
      )
    )
      fail(
        "COUNT.RELATIONSHIP",
        "typeRelationships",
        "relationship lacks captured directed source",
      );
  const outcomeKeys = new Map(outcomes.map((x) => [x, new Set()]));
  unique(
    dispositions.assertions,
    (x) => identity([x.kind, x.factRef]),
    "assertions",
  );
  for (const assertion of dispositions.assertions) {
    const entry = facts.get(assertion.factRef),
      fact = entry?.fact;
    if (
      !fact ||
      !["callBinding", "reference", "declarationBinding"].includes(fact.kind)
    )
      fail(
        "COUNT.DISPOSITION",
        "assertions",
        "assertion lacks a captured join fact",
      );
    const proof = proofFor(loaded, records, fact, entry.annotation);
    const candidate = anchored(loaded, measurement, fact.anchor);
    const capturedJoin = joins.joined.get(fact.ref)?.join;
    const status = capturedJoin?.status;
    if (
      !capturedJoin ||
      !same(capturedJoin.anchor, {
        document: fact.anchor.document,
        revisionId: fact.anchor.revisionId,
        contentHash: fact.anchor.contentHash,
        range: span(loaded, fact.anchor),
        kind: fact.anchor.kind,
      })
    )
      fail(
        "COUNT.DISPOSITION",
        "assertions",
        "disposition lacks its verified measured join",
      );
    const record =
      fact.kind === "reference"
        ? records.references.find((x) => x.provenanceId === proof.id)
        : fact.kind === "callBinding"
          ? records.callBindings.find((x) => x.provenanceId === proof.id)
          : records.declarationBindings.find(
              (x) => x.provenanceId === proof.id,
            );
    const diagnostic = records.referenceJoinDiagnostics.find(
      (x) => x.factRef === fact.ref && x.provenanceId === proof.id,
    );
    if (assertion.kind === "join") {
      if (
        assertion.disposition !== status ||
        (fact.kind === "reference" &&
          status === "exact" &&
          !exactReferences.has(fact.ref)) ||
        (fact.kind === "reference" &&
          status !== "exact" &&
          (!diagnostic || !same(diagnostic.join, capturedJoin))) ||
        (fact.kind !== "reference" &&
          (!record || !same(record.join, capturedJoin)))
      )
        fail(
          "COUNT.DISPOSITION",
          "assertions",
          "join conflicts with measured candidate or capability",
        );
      // Decision 0017 Amendment 1: unsupported is a per-document native
      // capability, so it counts each document with a declared-unavailable
      // family and a captured producer fact there, not each anchor.
      if (status === "unsupported")
        outcomeKeys
          .get("unsupported")
          .add(identity([fact.anchor.document, fact.anchor.revisionId]));
    } else {
      if (
        status !== "exact" ||
        !record ||
        !["reference", "callBinding"].includes(fact.kind) ||
        (fact.kind === "callBinding" &&
          (record.callId !== capturedJoin.candidateIds[0] ||
            !same(record.join, capturedJoin))) ||
        (fact.kind === "reference" && !exactReferences.has(fact.ref))
      )
        fail(
          "COUNT.DISPOSITION",
          "assertions",
          "resolution lacks an exact measured occurrence",
        );
      if (
        !same(
          record.declaredTarget,
          fact.record.declaredTarget === null
            ? null
            : target(loaded, measurement, fact.record.declaredTarget),
        ) ||
        !same(
          record.candidates,
          fact.record.candidates.map((x) => target(loaded, measurement, x)),
        ) ||
        record.resolution !== fact.record.resolution
      )
        fail(
          "COUNT.DISPOSITION",
          "assertions",
          "resolution disagrees with captured semantic target",
        );
      const expected =
        record.resolution === "external" ? "provenExternal" : record.resolution;
      if (assertion.disposition !== expected || !outcomeKeys.has(expected))
        fail(
          "COUNT.DISPOSITION",
          "assertions",
          "resolution contradicts captured exact proof",
        );
      // Decision 0017 Amendment 1: SCIP has no positive unresolved record, so
      // absence at an exact join is evidence only when coverage is complete.
      if (
        expected === "unresolved" &&
        !records.coverage.some(
          (row) =>
            row.producerId === proof.producerId &&
            row.sourceSetId === fact.anchor.document.sourceSetId &&
            row.documentPath === fact.anchor.document.path &&
            row.revisionId === fact.anchor.revisionId &&
            row.state === "complete",
        )
      )
        fail(
          "COUNT.DISPOSITION",
          "assertions",
          "unresolved outcome requires complete semantic coverage",
        );
      outcomeKeys
        .get(expected)
        .add(
          identity([
            fact.kind,
            fact.anchor.document,
            fact.anchor.revisionId,
            capturedJoin.candidateIds[0],
          ]),
        );
    }
  }
  const observed = new Set(
    [...exactReferences.values()].flatMap((x) => x.record.roles),
  );
  const directory = loaded.root?.replaceAll("\\", "/").split("/").at(-1);
  const literalExample = directory === "example";
  const languageDirectory = ["java", "rust", "python", "javascript"].includes(
    directory,
  );
  // Draft corpora check every invariant except the floors; the final corpus
  // profile always enforces them.
  const floorsEnforced = !(
    (fixture.profile === "example" && literalExample) ||
    (fixture.profile === "draft" && languageDirectory)
  );
  const count = {
    formatVersion: 1,
    language: fixture.language,
    profile: fixture.profile,
    floorsEnforced,
    scenariosTotal: scenarios.length,
    scenariosByCategory: categories.map((category) => ({
      category,
      count: scenarios.filter((x) => x.scenario.category === category).length,
    })),
    measuredCalls: records.calls.length,
    references: referenceOccurrences.size,
    callableValueNegatives: negativeOccurrences.size,
    typeRelationships: relationships.size,
    outcomes: outcomes.map((disposition) => ({
      disposition,
      count: outcomeKeys.get(disposition).size,
    })),
    observedRoles: roles.filter((role) => observed.has(role)),
  };
  validate("CountsV1", count);
  if (fixture.profile !== "corpus" && floorsEnforced)
    fail(
      "COUNT.PROFILE",
      "profile",
      "only literal example/ or a draft corpus in its language directory can bypass floors",
    );
  if (floorsEnforced) assertCorpusFloors(count);
  return count;
}

// Per-language corpus floors (Decision 0017 Amendment 1). Each language owns
// its row; nothing outside this table branches on language. `unsupported`
// counts documents, so its floor of one is the native-capability
// requirement.
const sharedFloors = {
  scenariosTotal: 32,
  scenariosPerCategory: 4,
  measuredCalls: 120,
  references: 40,
  callableValueNegatives: 20,
  typeRelationships: 20,
  outcomes: {
    resolved: 20,
    provenExternal: 20,
    ambiguous: 20,
    unresolved: 20,
    unsupported: 1,
  },
  exemptRoles: [],
};
const languageFloors = {
  javascript: sharedFloors,
  java: {
    ...sharedFloors,
    outcomes: { ...sharedFloors.outcomes, ambiguous: 0 },
    exemptRoles: ["alias"],
  },
  python: {
    ...sharedFloors,
    outcomes: { ...sharedFloors.outcomes, ambiguous: 0 },
    exemptRoles: ["alias"],
  },
  rust: {
    ...sharedFloors,
    typeRelationships: 0,
    outcomes: { ...sharedFloors.outcomes, ambiguous: 0 },
  },
};

// The count inventory is independently source-checked above. Keep the floor
// predicate separate for direct boundary checks and captured corpus integration.
export function assertCorpusFloors(count) {
  validate("CountsV1", count);
  const floors = languageFloors[count.language];
  const required = roles.filter((role) => !floors.exemptRoles.includes(role));
  if (
    count.scenariosTotal < floors.scenariosTotal ||
    count.scenariosByCategory.some(
      (x) => x.count < floors.scenariosPerCategory,
    ) ||
    count.measuredCalls < floors.measuredCalls ||
    count.references < floors.references ||
    count.callableValueNegatives < floors.callableValueNegatives ||
    count.typeRelationships < floors.typeRelationships ||
    count.outcomes.some((x) => x.count < floors.outcomes[x.disposition]) ||
    required.some((role) => !count.observedRoles.includes(role))
  )
    fail("COUNT.FLOOR", "counts", "corpus-profile minimum not met");
  return count;
}
