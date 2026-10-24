import { canonicalBytes } from "../json.mjs";
import { toByteRange } from "../coordinates.mjs";
import { validate } from "../formats.mjs";
import { applicableRoles } from "../lookup.mjs";
import { checkEnvelopeOrder, orderedEnvelope } from "./measurement.mjs";

const key = (value) => canonicalBytes(value).toString("hex");
const same = (a, b) => key(a) === key(b);
const compare = (a, b) => Buffer.compare(canonicalBytes(a), canonicalBytes(b));
function reject(assertion, field, message, code = "invalidRecord") {
  const error = new Error(`${assertion} ${field}: ${message}`);
  Object.assign(error, { assertion, field, code });
  throw error;
}
function position(source, range) {
  try {
    return toByteRange(source, range);
  } catch (error) {
    if (error.message?.includes("COORD.INVALID_RANGE"))
      reject("JOIN.TUPLE", "anchor.range", error.message, "invalidRange");
    throw error;
  }
}
function target(ref, measurements, context) {
  if (ref === null) return null;
  if (ref.kind === "external") {
    const symbol = ref.symbol;
    if ((symbol.scope === "document") !== (symbol.document !== null))
      reject(
        "REFERENCE.RESOLUTION",
        "declaredTarget",
        "external symbol scope differs",
      );
    return { kind: "external", symbol };
  }
  const row = measurements.recordByNativeRef.get(ref.declarationRef);
  if (
    !row ||
    !row.syntaxId ||
    row.revisionId !== ref.revisionId ||
    row.document.sourceSetId !== context.sourceSetId
  )
    reject(
      "REFERENCE.RESOLUTION",
      "declaredTarget",
      "internal target is not an admitted declaration in this source set",
    );
  return {
    kind: "internal",
    syntaxId: row.syntaxId,
    document: row.document,
    revisionId: row.revisionId,
  };
}
function targets(refs, measurements, context) {
  const values = refs.map((ref) => target(ref, measurements, context));
  if (
    values.some(
      (value, index) => index && compare(values[index - 1], value) >= 0,
    )
  )
    reject(
      "REFERENCE.RESOLUTION",
      "candidates",
      "candidates must be unique and canonically ordered",
    );
  return values;
}
function resolution(row) {
  const ok =
    (row.resolution === "resolved" &&
      row.declaredTarget?.kind === "internal" &&
      !row.candidates.length) ||
    (row.resolution === "external" &&
      row.declaredTarget?.kind === "external" &&
      !row.candidates.length) ||
    (row.resolution === "ambiguous" &&
      row.declaredTarget === null &&
      row.candidates.length >= 2) ||
    (row.resolution === "unresolved" &&
      row.declaredTarget === null &&
      !row.candidates.length);
  if (!ok)
    reject(
      "REFERENCE.RESOLUTION",
      "resolution",
      "invalid resolution cardinality",
    );
}
function roles(row, language, callee, declarationSite) {
  if (row.site === "declaration" && !declarationSite)
    reject(
      "REFERENCE.ROLES",
      "site",
      "declaration site requires a measured declaration name",
    );
  if (!row.roles.length)
    reject("REFERENCE.ROLES", "roles", "reference roles must be nonempty");
  const applicable = applicableRoles(language);
  let last = -1;
  for (const role of row.roles) {
    const index = applicable.indexOf(role);
    if (index <= last)
      reject(
        "REFERENCE.ROLES",
        "roles",
        "role repeated, inapplicable or out of order",
      );
    last = index;
  }
  if (
    (row.roles.includes("definition") && row.site !== "declaration") ||
    (row.roles.includes("alias") &&
      (row.site !== "declaration" || !row.roles.includes("definition")))
  )
    reject("REFERENCE.ROLES", "site", "role contradicts reference site");
  if (row.roles.includes("call") && !callee)
    reject(
      "REFERENCE.ROLES",
      "roles",
      "call role requires a measured callee at this reference",
    );
}
function closure(field, expected, actual, assertion) {
  checkEnvelopeOrder(field, actual);
  const ordered = orderedEnvelope(field, expected, { collapseIdentical: true });
  if (!same(ordered, actual))
    reject(assertion, field, "normalized membership or source claim differs");
}

// Native candidates come from the admitted source ranges and U2's measured identities;
// neither normalized joins nor an injected candidate index participates in this lookup.
export function checkJoins(loaded, records, coverage, measurement) {
  validate("NormalizedRecordsV1", records);
  validate("NativeArtifact", loaded.native);
  if (
    !(coverage?.semanticProofsById instanceof Map) ||
    typeof coverage.checkUse !== "function"
  )
    reject("JOIN.TUPLE", "provenanceId", "verified coverage required");
  if (
    !(measurement?.identityByRef instanceof Map) ||
    !(measurement.recordByNativeRef instanceof Map)
  )
    reject("JOIN.TUPLE", "anchor", "verified measurement inventory required");
  const native = loaded.native,
    nativeProducer = loaded.fixture.producers.find(
      (p) => p.id === native.producerId && p.kind === "native",
    );
  if (!nativeProducer)
    reject("JOIN.TUPLE", "anchor", "admitted native producer required");
  const measured = new Map(),
    byNativeRef = new Set();
  const declarations = new Map(
    native.declarations.map((row) => [row.ref, row]),
  );
  for (const row of [
    ...native.declarations,
    ...native.calls,
    ...native.controls,
    ...native.references,
  ]) {
    if (byNativeRef.has(row.ref))
      reject("JOIN.CARDINALITY", "anchor", "duplicate native reference");
    byNativeRef.add(row.ref);
    const source = loaded.sources.get(
      JSON.stringify([
        row.document.sourceSetId,
        row.revisionId,
        row.document.path,
      ]),
    );
    const document = loaded.revisions
      .get(JSON.stringify([row.document.sourceSetId, row.revisionId]))
      ?.documents.find((x) => same(x.key, row.document));
    if (!source || !document)
      reject("JOIN.TUPLE", "anchor", "native row outside admitted source");
    const id = measurement.identityByRef.get(row.ref);
    if (!id)
      reject(
        "JOIN.CARDINALITY",
        "anchor",
        "native row has no verified identity",
      );
    const common = {
      document: row.document,
      revisionId: row.revisionId,
      contentHash: document.contentHash,
    };
    const add = (kind, span, ownerRef) => {
      if (span === null) return;
      if (span.encoding !== nativeProducer.positionEncoding)
        reject(
          "JOIN.TUPLE",
          "anchor.range",
          "native position encoding differs",
        );
      const anchor = { ...common, range: position(source, span), kind };
      const tuple = key([anchor, ownerRef]);
      if (!measured.has(tuple)) measured.set(tuple, []);
      measured.get(tuple).push({ id, ref: row.ref });
    };
    if (declarations.has(row.ref)) {
      if (row.name !== null)
        add("declarationName", row.nameRange, row.parentRef ?? row.ref);
    } else if (native.calls.includes(row)) {
      add("invocation", row.range, row.ownerRef);
      add("callee", row.calleeRange, row.ownerRef);
    } else if (native.references.includes(row))
      add("reference", row.range, row.ownerRef);
  }
  const capabilityByTuple = new Map();
  const reports = [...native.capabilityReports];
  if (native.producerId !== "trellis.native.syntax")
    for (const intent of loaded.fixture.coverageIntents.filter((entry) => entry.producerId === native.producerId))
      if (!reports.some((report) => report.revisionId === intent.revisionId && same(report.document, intent.document)))
        reports.push({ document: intent.document, revisionId: intent.revisionId, families: intent.measurementSupport });
  for (const report of reports) {
    const tuple = key([report.document, report.revisionId]);
    const revision = loaded.revisions.get(JSON.stringify([report.document.sourceSetId, report.revisionId]));
    const document = revision?.documents.find((x) => same(x.key, report.document));
    const source = loaded.sources.get(JSON.stringify([report.document.sourceSetId, report.revisionId, report.document.path]));
    if (!document || !source || capabilityByTuple.has(tuple))
      reject("JOIN.SUPPORT", "capabilityReports", "duplicate or unadmitted report tuple");
    if (report.families.length !== 4 || new Set(report.families.map((entry) => entry.kind)).size !== 4)
      reject("JOIN.SUPPORT", "capabilityReports", "four unique families required");
    const intent = loaded.fixture.coverageIntents.find((entry) => entry.producerId === native.producerId && entry.revisionId === report.revisionId && same(entry.document, report.document));
    const ordered = (entries) => [...entries].sort((a, b) => a.kind.localeCompare(b.kind));
    if (intent && !same(ordered(intent.measurementSupport), ordered(report.families)))
      reject("JOIN.SUPPORT", "measurementSupport", "intent does not mirror captured report");
    const supports = new Map();
    for (const entry of report.families) {
      if ((native.producerId === "trellis.native.syntax" && !entry.available) || entry.available !== (entry.diagnostic === null) || (!entry.available && entry.gaps.length))
        reject("JOIN.SUPPORT", "capabilityReports", "invalid family availability or diagnostic");
      const gaps = entry.gaps.map((gap) => {
        if (native.producerId === "trellis.native.syntax" && gap.range.encoding !== "utf8")
          reject("JOIN.SUPPORT", "capabilityReports", "production gap coordinates must be UTF-8");
        const range = position(source, gap.range);
        if (range.start >= range.end)
          reject("JOIN.SUPPORT", "capabilityReports", "gap must be nonempty");
        return { range, diagnostic: gap.diagnostic };
      });
      for (const encoded of measured.keys()) {
        const [anchor] = JSON.parse(Buffer.from(encoded, "hex").toString("utf8"));
        if (anchor.kind !== entry.kind || anchor.revisionId !== report.revisionId || !same(anchor.document, report.document)) continue;
        if ((native.capabilityReports.includes(report) && !entry.available) || gaps.some(({ range }) => range.start <= anchor.range.start && anchor.range.end <= range.end))
          reject("JOIN.SUPPORT", "measurementSupport", "candidate is wholly inside claimed unavailable capability");
      }
      supports.set(entry.kind, { ...entry, gaps });
    }
    capabilityByTuple.set(tuple, supports);
  }
  if (native.producerId === "trellis.native.syntax")
    for (const revision of loaded.fixture.revisions)
      for (const document of revision.documents)
        if (!capabilityByTuple.has(key([document.key, revision.id])))
          reject("JOIN.SUPPORT", "capabilityReports", "production document report missing");
  const verifiedProofs = new Map(records.provenance.map((x) => [x.id, x]));
  const nativeReferences = new Map(
    native.references.map((row) => [row.ref, row]),
  );
  const facts = loaded.annotations.flatMap((annotation) =>
    annotation.facts
      .filter((f) =>
        ["declarationBinding", "callBinding", "reference"].includes(f.kind),
      )
      .map((f) => ({ fact: f, annotation })),
  );
  const joined = new Map(),
    recordByFactRef = new Map(),
    expectedDiagnostics = [],
    expectedReferences = [],
    seenFacts = new Set(),
    referenceClaims = new Map();
  for (const { fact, annotation } of facts) {
    if (seenFacts.has(fact.ref))
      reject(
        "JOIN.CARDINALITY",
        "factRef",
        "duplicate semantic fact reference",
      );
    seenFacts.add(fact.ref);
    const family = fact.anchor.kind;
    if (
      (fact.kind === "declarationBinding" && family !== "declarationName") ||
      (fact.kind === "callBinding" &&
        !["callee", "invocation"].includes(family)) ||
      (fact.kind === "reference" && family !== "reference")
    )
      reject("JOIN.FAMILY", "anchor.kind", "fact anchor family differs");
    const proof = verifiedProofs.get(fact.record.provenanceId);
    const captured = coverage.semanticProofsById.get(fact.record.provenanceId);
    const producer = loaded.fixture.producers.find(
      (x) => x.id === proof?.producerId && x.kind === "semantic",
    );
    if (
      !proof ||
      !captured ||
      !producer ||
      proof.evidenceKind !==
        (fact.kind === "declarationBinding"
          ? "declarationBinding"
          : "semanticReference") ||
      !same(proof.document, annotation.document) ||
      proof.revisionId !== annotation.revisionId ||
      !same(
        (({ freshness, ...row }) => row)(captured),
        (({ freshness, ...row }) => row)(proof),
      )
    )
      reject(
        "JOIN.TUPLE",
        "provenanceId",
        "unverified producer/proof/fact source tuple",
      );
    const selector = fact.anchor;
    if (
      !same(selector.document, annotation.document) ||
      selector.revisionId !== annotation.revisionId ||
      !same(selector.document, proof.document) ||
      selector.revisionId !== proof.revisionId ||
      selector.contentHash !== proof.contentHash
    )
      reject(
        "JOIN.TUPLE",
        "anchor",
        `semantic anchor disagrees with captured proof${proof.contentHash !== selector.contentHash ? `: expected contentHash ${proof.contentHash}, actual ${selector.contentHash}` : ""}`,
      );
    // Coverage selection belongs to the semantic producer, not the native measurement.
    // Verify every fact, including those that will remain unmatched or unsupported.
    coverage.checkUse({
      producerId: proof.producerId,
      document: selector.document,
      revisionId: selector.revisionId,
      provenanceIds: [proof.id],
    });
    if (selector.range.encoding !== producer.positionEncoding)
      reject(
        "JOIN.TUPLE",
        "anchor.range",
        "semantic producer encoding differs",
      );
    const source = loaded.sources.get(
      JSON.stringify([
        selector.document.sourceSetId,
        selector.revisionId,
        selector.document.path,
      ]),
    );
    const document = loaded.revisions
      .get(JSON.stringify([selector.document.sourceSetId, selector.revisionId]))
      ?.documents.find((x) => same(x.key, selector.document));
    if (!source || !document || selector.contentHash !== document.contentHash)
      reject(
        "JOIN.TUPLE",
        "anchor",
        `source set, document, revision or hash differs: expected ${document?.contentHash ?? "a captured document"}, actual ${selector.contentHash}`,
      );
    const owner = declarations.get(selector.ownerRef);
    if (
      !owner ||
      !same(owner.document, selector.document) ||
      owner.revisionId !== selector.revisionId
    )
      reject(
        "JOIN.OWNER",
        "anchor.ownerRef",
        "anchor owner is not a declaration in its source snapshot",
      );
    const anchor = {
      document: selector.document,
      revisionId: selector.revisionId,
      contentHash: selector.contentHash,
      range: position(source, selector.range),
      kind: family,
    };
    const ownerRange = position(source, owner.range);
    if (
      anchor.range.start < ownerRange.start ||
      anchor.range.end > ownerRange.end
    )
      reject(
        "JOIN.OWNER",
        "anchor.ownerRef",
        "anchor falls outside its admitted measured owner",
      );
    const intent = loaded.fixture.coverageIntents.find(
      (x) =>
        x.producerId === native.producerId &&
        x.revisionId === selector.revisionId &&
        same(x.document, selector.document),
    );
    const capturedSupport = capabilityByTuple.get(key([selector.document, selector.revisionId]))?.get(family);
    const support = capturedSupport ?? (native.producerId === "trellis.native.syntax" ? null : intent?.measurementSupport.find((x) => x.kind === family));
    if (!support)
      reject(
        "JOIN.SUPPORT",
        "measurementSupport",
        "native family capability absent",
      );
    // Ownership is part of the exact key; it cannot be guessed from spelling or overlap.
    const matches = measured.get(key([anchor, selector.ownerRef])) ?? [];
    const candidateIds = [...new Set(matches.map((x) => x.id))].sort((a, b) =>
      Buffer.compare(Buffer.from(a), Buffer.from(b)),
    );
    if (!support.available && matches.length)
      reject(
        "JOIN.SUPPORT",
        "measurementSupport",
        "unavailable family has native candidates",
      );
    if (
      (support.available && support.diagnostic !== null) ||
      (!support.available && support.diagnostic === null)
    )
      reject(
        "JOIN.SUPPORT",
        "measurementSupport",
        "family availability/diagnostic differs",
      );
    const gap = support.gaps.find((gap) => {
      const range = capturedSupport ? gap.range : position(source, gap.range);
      return range.start <= anchor.range.start && anchor.range.end <= range.end;
    });
    if (gap && matches.length)
      reject("JOIN.SUPPORT", "measurementSupport", "gap contradicts measured candidate");
    const status = !support.available || gap
      ? "unsupported"
      : candidateIds.length === 0
        ? "unmatched"
        : candidateIds.length === 1
          ? "exact"
          : "ambiguous";
    const join = {
      anchor,
      status,
      candidateIds: status === "unsupported" ? [] : candidateIds,
      diagnostic:
        status === "exact"
          ? null
          : status === "unsupported"
            ? gap?.diagnostic ?? support.diagnostic
            : status,
    };
    const nativeRefs = status === "exact" ? matches.map((x) => x.ref) : [];
    joined.set(fact.ref, {
      join,
      installedId: status === "exact" ? candidateIds[0] : null,
      producerId: proof.producerId,
      provenanceIds: [proof.id],
      nativeRefs,
    });
    if (fact.kind !== "reference") {
      recordByFactRef.set(fact.ref, join);
      continue;
    }
    if (status !== "exact") {
      const diagnostic = { factRef: fact.ref, provenanceId: proof.id, join };
      expectedDiagnostics.push(diagnostic);
      recordByFactRef.set(fact.ref, diagnostic);
      continue;
    }
    const row = nativeReferences.get(matches[0].ref),
      descriptor = measurement.nativeReferenceDescriptors.find(
        (x) => x.ref === row?.ref,
      );
    if (
      !row ||
      !descriptor ||
      descriptor.id !== candidateIds[0] ||
      !same(descriptor.range, anchor.range)
    )
      reject(
        "REFERENCE.SOURCE",
        "id",
        "native reference lacks verified measured identity",
      );
    const value = {
      id: descriptor.id,
      ownerSyntaxId: descriptor.ownerSyntaxId,
      ordinal: descriptor.ordinal,
      document: row.document,
      revisionId: row.revisionId,
      range: anchor.range,
      spelling: row.spelling,
      lookupKey: descriptor.lookupKey,
      site: fact.record.site,
      roles: fact.record.roles,
      resolution: fact.record.resolution,
      declaredTarget: target(
        fact.record.declaredTarget,
        measurement,
        proof.document,
      ),
      candidates: targets(fact.record.candidates, measurement, proof.document),
      provenanceId: proof.id,
    };
    const callee = native.calls.some(
      (call) =>
        call.ownerRef === row.ownerRef &&
        same(call.document, row.document) &&
        call.revisionId === row.revisionId &&
        call.calleeRange !== null &&
        same(position(source, call.calleeRange), anchor.range),
    );
    const declarationSite = (
      measured.get(
        key([{ ...anchor, kind: "declarationName" }, selector.ownerRef]),
      ) ?? []
    ).some(
      (candidate) =>
        declarations.has(candidate.ref) &&
        same(declarations.get(candidate.ref).document, row.document) &&
        declarations.get(candidate.ref).revisionId === row.revisionId,
    );
    roles(value, row.document.language, callee, declarationSite);
    resolution(value);
    validate("Reference", value);
    const group = key([proof.producerId, value.revisionId, value.id]);
    if (!referenceClaims.has(group)) referenceClaims.set(group, []);
    referenceClaims.get(group).push({ factRef: fact.ref, value });
  }
  for (const entries of referenceClaims.values()) {
    const baseline = entries[0].value;
    const strip = ({
      resolution,
      declaredTarget,
      candidates,
      provenanceId,
      ...rest
    }) => rest;
    if (entries.some(({ value }) => !same(strip(value), strip(baseline))))
      reject(
        "REFERENCE.SOURCE",
        "references",
        "contributor non-target claims differ",
      );
    const claims = entries.map(({ value }) => value.declaredTarget);
    const distinct = [
      ...new Map(claims.map((value) => [key(value), value])).values(),
    ];
    const contradiction = distinct.length > 1;
    if (
      contradiction &&
      entries.some(
        ({ value }) =>
          !["resolved", "external"].includes(value.resolution) ||
          value.declaredTarget === null ||
          value.candidates.length,
      )
    )
      reject(
        "REFERENCE.SOURCE",
        "references",
        "incompatible duplicate semantic claims",
      );
    if (
      !contradiction &&
      entries.some(
        ({ value }) =>
          !same(
            (({ provenanceId, ...rest }) => rest)(value),
            (({ provenanceId, ...rest }) => rest)(baseline),
          ),
      )
    )
      reject(
        "REFERENCE.SOURCE",
        "references",
        "incompatible duplicate semantic claims",
      );
    const candidates = contradiction ? distinct.sort(compare) : null;
    for (const { factRef, value } of entries) {
      const member = contradiction
        ? {
            ...value,
            resolution: "ambiguous",
            declaredTarget: null,
            candidates,
          }
        : value;
      validate("Reference", member);
      expectedReferences.push(member);
      recordByFactRef.set(factRef, member);
    }
  }
  // A diagnostic is the sole normalized trace of a non-exact reference. Exact
  // references have no diagnostic and cannot be created by a non-exact fact.
  closure(
    "referenceJoinDiagnostics",
    expectedDiagnostics,
    records.referenceJoinDiagnostics,
    "JOIN.DIAGNOSTIC",
  );
  closure(
    "references",
    expectedReferences,
    records.references,
    "REFERENCE.SOURCE",
  );
  return {
    joined,
    referenceClaims,
    expectedReferences: orderedEnvelope("references", expectedReferences, {
      collapseIdentical: true,
    }),
    expectedDiagnostics: orderedEnvelope(
      "referenceJoinDiagnostics",
      expectedDiagnostics,
      { collapseIdentical: true },
    ),
    recordByFactRef,
  };
}
