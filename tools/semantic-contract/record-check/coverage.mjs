import { validate } from "../formats.mjs";
import { canonicalBytes } from "../json.mjs";
import { contentHash, sourceManifestHash } from "../identity.mjs";
import { applicableRoles } from "../lookup.mjs";

const encode = (value) => canonicalBytes(value).toString("hex");
const key = (producerId, document, revisionId) =>
  encode([
    producerId,
    document.sourceSetId,
    document.language,
    document.path,
    revisionId,
  ]);
const documentKey = (document, revisionId) =>
  encode([document.sourceSetId, document.language, document.path, revisionId]);
const snapshotKey = (sourceSetId, revisionId) =>
  JSON.stringify([sourceSetId, revisionId]);
const equal = (a, b) => encode(a) === encode(b);
const order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
function fail(assertion, field, message) {
  const error = new Error(`${assertion} ${field}: ${message}`);
  error.assertion = assertion;
  error.code = "invalidRecord";
  error.field = field;
  throw error;
}
function requireEqual(a, b, assertion, field) {
  if (!equal(a, b))
    fail(assertion, field, "record differs from admitted input");
}
function unique(rows, select, assertion, field) {
  const result = new Map();
  for (const row of rows) {
    const id = select(row);
    if (result.has(id)) fail(assertion, field, "duplicate tuple or identity");
    result.set(id, row);
  }
  return result;
}
const families = (role) =>
  role === "definition" || role === "alias"
    ? ["reference", "declarationName"]
    : ["reference"];
function sortedRoles(roles, language, field) {
  const allowed = applicableRoles(language);
  let previous = -1;
  for (const role of roles) {
    const index = allowed.indexOf(role);
    if (index <= previous)
      fail(
        "COVERAGE.ROLES",
        field,
        "role is inapplicable, repeated or unordered",
      );
    previous = index;
  }
}
// Actual digests of the captured bytes of one kind, for mismatch diagnostics.
function capturedHashes(loaded, kind) {
  return loaded.fixture.captures
    .filter((x) => x.kind === kind && loaded.captureBytes.has(x.ref))
    .map((x) => contentHash(loaded.captureBytes.get(x.ref)));
}
const oneOf = (hashes, actual) =>
  `expected one of [${hashes.join(", ")}], actual ${actual}`;
function capture(loaded, kind, hash) {
  return loaded.fixture.captures.some(
    (x) =>
      x.kind === kind &&
      x.hash === hash &&
      (x.pinned !== undefined ||
        (loaded.captureBytes.has(x.ref) &&
          contentHash(loaded.captureBytes.get(x.ref)) === hash)),
  );
}
function documentAt(loaded, document, revisionId) {
  return loaded.revisions
    .get(snapshotKey(document.sourceSetId, revisionId))
    ?.documents.find((x) => equal(x.key, document));
}
function capturedBasis(proof, loaded, proofRow) {
  validate("Provenance", proof);
  const document = documentAt(loaded, proof.document, proof.revisionId);
  if (!document || document.contentHash !== proof.contentHash)
    fail(
      "FRESHNESS.BASIS",
      "contentHash",
      `captured document bytes disagree: expected ${document?.contentHash ?? "a captured document"}, actual ${proof.contentHash}`,
    );
  const producer = loaded.fixture.producers.find(
    (x) => x.id === proof.producerId,
  );
  if (
    !producer ||
    !producer.languages.includes(proof.document.language) ||
    !capture(loaded, "executable", producer.executableHash)
  )
    fail(
      "FRESHNESS.BASIS",
      "producerId",
      `captured executable or producer missing${producer ? `: ${oneOf(capturedHashes(loaded, "executable"), producer.executableHash)}` : ""}`,
    );
  if (producer.kind === "native") {
    if (proof.evidenceKind !== "measuredSyntax" || proof.basis !== null)
      fail("FRESHNESS.BASIS", "basis", "native proof requires null basis");
    return producer;
  }
  if (proof.evidenceKind === "measuredSyntax" || proof.basis === null)
    fail("FRESHNESS.BASIS", "basis", "semantic proof requires a basis");
  const basis = proof.basis,
    revision = loaded.revisions.get(
      snapshotKey(proof.document.sourceSetId, proof.revisionId),
    );
  const expected = {
    producerId: producer.id,
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    language: proof.document.language,
    sourceSetId: proof.document.sourceSetId,
    revisionId: proof.revisionId,
    sourceManifestHash: sourceManifestHash(
      revision.documents.map((x) => ({
        document: x.key,
        contentHash: x.contentHash,
      })),
    ),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
  };
  for (const [field, value] of Object.entries(expected))
    if (basis[field] !== value)
      fail(
        "FRESHNESS.BASIS",
        `basis.${field}`,
        "captured self-relation differs",
      );
  for (const [field, kind] of [
    ["toolchainHash", "toolchain"],
    ["configHash", "config"],
    ["dependencyHash", "dependency"],
  ])
    if (!capture(loaded, kind, basis[field]))
      fail(
        "FRESHNESS.BASIS",
        `basis.${field}`,
        `captured bytes missing or changed: ${oneOf(capturedHashes(loaded, kind), basis[field])}`,
      );
  if (
    !capture(loaded, "semanticArtifact", basis.artifactHash) ||
    !loaded.semanticBytes.some(
      (x) =>
        x.value.producerId === producer.id &&
        contentHash(x.bytes) === basis.artifactHash,
    )
  )
    fail(
      "FRESHNESS.BASIS",
      "basis.artifactHash",
      `artifact capture missing or changed: ${oneOf(
        loaded.semanticBytes
          .filter((x) => x.value.producerId === producer.id)
          .map((x) => contentHash(x.bytes)),
        basis.artifactHash,
      )}`,
    );
  if (
    basis.lookupDependencies.some((x) =>
      /^(?:sid|occ):v1:[a-f0-9]{32}$/.test(x),
    ) ||
    basis.lookupDependencies.some(
      (x, i) => i && order(basis.lookupDependencies[i - 1], x) >= 0,
    )
  )
    fail(
      "FRESHNESS.BASIS",
      "basis.lookupDependencies",
      "lookup keys must be ordered, unique, and not identity IDs",
    );
  if (
    !proofRow ||
    proofRow.hash !== basis.artifactHash ||
    !equal(
      (({ freshness, ...captured }) => captured)(proofRow.wrapper),
      (({ freshness, ...captured }) => captured)(proof),
    )
  )
    fail(
      "FRESHNESS.BASIS",
      "provenance",
      `proof does not match captured fact and wrapper${proofRow?.hash !== basis.artifactHash ? `: expected artifactHash ${proofRow?.hash}, actual ${basis.artifactHash}` : ""}`,
    );
  return producer;
}
// Compare authenticated capture values with the selected, admitted request. This policy
// also accepts a null requested component when a requested value is unavailable.
export function compareFreshnessComponents(captured, requested) {
  if (
    requested.documentHash === null ||
    requested.documentHash !== captured.documentHash
  )
    return "stale";
  for (const field of [
    "revisionId",
    "sourceManifestHash",
    "toolchainHash",
    "configHash",
    "dependencyHash",
    "sourceSetId",
    "producerId",
    "producerKind",
    "producerVersion",
    "producerHash",
    "language",
    "positionEncoding",
    "producerLanguages",
  ]) {
    if (requested[field] === null || !equal(requested[field], captured[field]))
      return "possiblyStale";
  }
  return "fresh";
}
export function assertFreshnessComponents(captured, requested, claimed) {
  const actual = compareFreshnessComponents(captured, requested);
  if (claimed !== actual)
    fail("FRESHNESS.STATE", "freshness", "incorrect freshness label");
  return actual;
}
function freshness(proof, loaded, producer) {
  const selected = loaded.revisions.get(
    snapshotKey(loaded.comparison.sourceSetId, loaded.comparison.revisionId),
  );
  const wanted = selected?.documents.find(
    (x) =>
      x.key.path === proof.document.path &&
      x.key.language === proof.document.language,
  );
  if (producer.kind === "native") {
    const captured = {
      documentHash: proof.contentHash,
      revisionId: proof.revisionId,
      sourceSetId: proof.document.sourceSetId,
      sourceManifestHash: "native",
      toolchainHash: "native",
      configHash: "native",
      dependencyHash: "native",
      producerId: producer.id,
      producerKind: producer.kind,
      producerVersion: producer.version,
      producerHash: producer.executableHash,
      language: proof.document.language,
      positionEncoding: producer.positionEncoding,
      producerLanguages: producer.languages,
    };
    const requested = {
      ...captured,
      documentHash: wanted?.contentHash ?? null,
      revisionId: selected?.id ?? null,
      sourceSetId: loaded.comparison.sourceSetId,
    };
    return assertFreshnessComponents(captured, requested, proof.freshness);
  }
  const basis = proof.basis,
    requestedProducer = loaded.comparison.producers.find(
      (x) => x.id === basis.producerId,
    );
  const captured = {
    documentHash: proof.contentHash,
    revisionId: basis.revisionId,
    sourceManifestHash: basis.sourceManifestHash,
    toolchainHash: basis.toolchainHash,
    configHash: basis.configHash,
    dependencyHash: basis.dependencyHash,
    sourceSetId: basis.sourceSetId,
    producerId: producer.id,
    producerKind: producer.kind,
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    language: proof.document.language,
    positionEncoding: producer.positionEncoding,
    producerLanguages: producer.languages,
  };
  const requested = {
    documentHash: wanted?.contentHash ?? null,
    revisionId: selected?.id ?? null,
    sourceManifestHash: selected
      ? sourceManifestHash(
          selected.documents.map((x) => ({
            document: x.key,
            contentHash: x.contentHash,
          })),
        )
      : null,
    toolchainHash: selected?.toolchainHash ?? null,
    configHash: selected?.configHash ?? null,
    dependencyHash: selected?.dependencyHash ?? null,
    sourceSetId: loaded.comparison.sourceSetId,
    producerId: requestedProducer?.id ?? null,
    producerKind: requestedProducer?.kind ?? null,
    producerVersion: requestedProducer?.version ?? null,
    producerHash: requestedProducer?.executableHash ?? null,
    language: requestedProducer?.languages.includes(proof.document.language)
      ? proof.document.language
      : null,
    positionEncoding: requestedProducer?.positionEncoding ?? null,
    producerLanguages: requestedProducer?.languages ?? null,
  };
  // A requested executable without matching captured bytes is unavailable, not fresh.
  if (
    requestedProducer &&
    !capture(loaded, "executable", requestedProducer.executableHash)
  )
    requested.producerHash = null;
  return assertFreshnessComponents(captured, requested, proof.freshness);
}
function coverageState(row, intent, nativeProducerId) {
  const requested = intent.requestedRoles,
    support = new Map(
      intent.measurementSupport.map((x) => [x.kind, x.available]),
    );
  // A call may be joined at either its measured callee or invocation span.
  // Unavailable support for one family does not invalidate a fact in the other.
  const available = (role) =>
    role === "call"
      ? support.get("callee") === true || support.get("invocation") === true
      : families(role).every((x) => support.get(x) === true);
  const effective = requested.filter(
    (x) => row.supportedRoles.includes(x) && available(x),
  );
  const unsupported = requested.some(
    (x) => !row.supportedRoles.includes(x) || !available(x),
  );
  const missing = effective.some((x) => !row.observedRoles.includes(x));
  if (row.observedRoles.some((x) => !effective.includes(x)))
    fail(
      "COVERAGE.ROLES",
      "coverage.observedRoles",
      "role is not requested, supported and available",
    );
  if (!row.requested) return "notRequested";
  if (!row.selected) {
    if (row.observedRoles.length)
      fail(
        "COVERAGE.ROLES",
        "coverage.observedRoles",
        "unselected tuple cannot observe selected evidence",
      );
    return effective.length ? "omitted" : "unsupported";
  }
  if (row.state === "failed" && !row.observedRoles.length) return "failed";
  const nativeGap = row.producerId === nativeProducerId && requested.some((role) => {
    const required = role === "call" ? ["callee", "invocation"] : families(role);
    return required.some((kind) => intent.measurementSupport.some((entry) => entry.kind === kind && entry.gaps.length > 0));
  });
  return unsupported || missing || nativeGap ? "partial" : "complete";
}

export function checkCoverage(loaded, records) {
  validate("NormalizedRecordsV1", records);
  if (records.formatVersion !== loaded.fixture.formatVersion)
    fail("RECORDS.IDENTITY", "formatVersion", "version differs");
  requireEqual(
    records.comparison,
    loaded.fixture.comparison,
    "RECORDS.IDENTITY",
    "comparison",
  );
  const expectedProducers = unique(
    loaded.fixture.producers,
    (x) => x.id,
    "RECORDS.IDENTITY",
    "producers",
  );
  const actualProducers = unique(
    records.producers,
    (x) => x.id,
    "RECORDS.IDENTITY",
    "producers",
  );
  const expectedSets = unique(
    loaded.fixture.sourceSets,
    (x) => x.id,
    "RECORDS.IDENTITY",
    "sourceSets",
  );
  const actualSets = unique(
    records.sourceSets,
    (x) => x.id,
    "RECORDS.IDENTITY",
    "sourceSets",
  );
  const expectedRevisions = new Map();
  for (const revision of loaded.fixture.revisions) {
    const source = loaded.revisions.get(
      snapshotKey(revision.sourceSetId, revision.id),
    );
    if (!source)
      fail("RECORDS.IDENTITY", "revisions", "admitted snapshot missing");
    const documents = revision.documents.map((item) => {
      const bytes = loaded.sources.get(
        JSON.stringify([revision.sourceSetId, revision.id, item.key.path]),
      );
      if (!bytes)
        fail("RECORDS.IDENTITY", "revisions.documents", "source bytes missing");
      return {
        key: item.key,
        revisionId: item.revisionId,
        contentHash: contentHash(bytes),
        byteLength: bytes.length,
      };
    });
    expectedRevisions.set(snapshotKey(revision.sourceSetId, revision.id), {
      ...revision,
      documents,
    });
  }
  const actualRevisions = unique(
    records.revisions,
    (x) => snapshotKey(x.sourceSetId, x.id),
    "RECORDS.IDENTITY",
    "revisions",
  );
  for (const [field, want, got] of [
    ["producers", expectedProducers, actualProducers],
    ["sourceSets", expectedSets, actualSets],
    ["revisions", expectedRevisions, actualRevisions],
  ]) {
    if (want.size !== got.size)
      fail("RECORDS.IDENTITY", field, "identity inventory differs");
    for (const [id, row] of want) {
      if (!got.has(id))
        fail("RECORDS.IDENTITY", field, "missing admitted identity");
      requireEqual(got.get(id), row, "RECORDS.IDENTITY", field);
    }
    const ordered = [...got.values()];
    if (
      ordered.some(
        (x, i) =>
          i &&
          Buffer.compare(canonicalBytes(ordered[i - 1]), canonicalBytes(x)) > 0,
      )
    )
      fail("RECORDS.IDENTITY", field, "identity inventory unordered");
  }
  const intents = unique(
    loaded.fixture.coverageIntents,
    (x) => key(x.producerId, x.document, x.revisionId),
    "COVERAGE.TUPLE",
    "coverage",
  );
  const expected = new Map();
  for (const producer of loaded.fixture.producers)
    for (const revision of expectedRevisions.values())
      for (const document of revision.documents) {
        if (!producer.languages.includes(document.key.language)) continue;
        const id = key(producer.id, document.key, revision.id),
          intent = intents.get(id);
        if (!intent)
          fail("COVERAGE.TUPLE", "coverage", "missing admitted intent");
        expected.set(id, { producer, revision, document, intent });
      }
  if (intents.size !== expected.size)
    fail("COVERAGE.TUPLE", "coverage", "extra or missing intent");
  const coverageByTuple = unique(
    records.coverage,
    (x) =>
      key(
        x.producerId,
        {
          sourceSetId: x.sourceSetId,
          language: x.language,
          path: x.documentPath,
        },
        x.revisionId,
      ),
    "COVERAGE.TUPLE",
    "coverage",
  );
  const authoredCoverage = new Map();
  for (const annotation of loaded.annotations)
    for (const fact of annotation.facts) {
      if (fact.kind !== "coverage") continue;
      const row = fact.record,
        id = key(
          row.producerId,
          {
            sourceSetId: row.sourceSetId,
            language: row.language,
            path: row.documentPath,
          },
          row.revisionId,
        );
      if (
        row.sourceSetId !== annotation.document.sourceSetId ||
        row.language !== annotation.document.language ||
        row.documentPath !== annotation.document.path ||
        row.revisionId !== annotation.revisionId ||
        authoredCoverage.has(id)
      )
        fail(
          "COVERAGE.TUPLE",
          "coverage",
          "coverage fact has wrong annotation tuple or duplicates it",
        );
      authoredCoverage.set(id, row);
    }
  if (
    coverageByTuple.size !== expected.size ||
    authoredCoverage.size !== expected.size
  )
    fail("COVERAGE.TUPLE", "coverage", "missing or extra coverage tuple");
  for (const [id, { intent }] of expected) {
    const row = coverageByTuple.get(id);
    if (!row) fail("COVERAGE.TUPLE", "coverage", "missing coverage tuple");
    if (row.selected !== authoredCoverage.get(id)?.selected)
      fail(
        "COVERAGE.STATE",
        "coverage",
        "selected flag differs from captured intent",
      );
    sortedRoles(
      intent.requestedRoles,
      intent.document.language,
      "coverage.requestedRoles",
    );
    sortedRoles(row.supportedRoles, row.language, "coverage.supportedRoles");
    sortedRoles(row.observedRoles, row.language, "coverage.observedRoles");
    const expectedRequested = intent.requestedRoles.length > 0;
    if (row.requested !== expectedRequested || (row.selected && !row.requested))
      fail(
        "COVERAGE.STATE",
        "coverage",
        "requested or selected contradicts intent",
      );
    const state = coverageState(row, intent, loaded.native.producerId);
    if (
      row.state !== state ||
      (row.diagnostic === null) !== ["notRequested", "complete"].includes(state)
    )
      fail(
        "COVERAGE.STATE",
        "coverage",
        `state or diagnostic contradicts source evidence: ${row.producerId}/${row.revisionId}/${row.documentPath} ${row.state} expected ${state}`,
      );
    const authored = authoredCoverage.get(id);
    if (!authored)
      fail(
        "COVERAGE.TUPLE",
        "coverage",
        "coverage row not present in captured annotation",
      );
    for (const field of [
      "requested",
      "selected",
      "state",
      "supportedRoles",
      "observedRoles",
      "diagnostic",
    ])
      if (!equal(row[field], authored[field]))
        fail(
          field.endsWith("Roles") ? "COVERAGE.ROLES" : "COVERAGE.STATE",
          field.endsWith("Roles") ? `coverage.${field}` : "coverage",
          "normalized coverage differs from captured claim",
        );
  }
  const semanticProofsById = new Map();
  for (const annotation of loaded.annotations)
    for (const fact of annotation.facts) {
      if (fact.kind !== "provenance" || fact.record.basis === null) continue;
      const row = fact.record;
      if (
        semanticProofsById.has(row.id) &&
        !equal(semanticProofsById.get(row.id), row)
      )
        fail(
          "FRESHNESS.BASIS",
          "provenance",
          "conflicting captured semantic proofs",
        );
      semanticProofsById.set(row.id, row);
    }
  for (const row of semanticProofsById.values())
    capturedBasis(row, loaded, loaded.semanticProofs.get(row.id));
  const checkedProofs = new Map();
  for (const proof of records.provenance) {
    if (checkedProofs.has(proof.id))
      fail("FRESHNESS.BASIS", "provenance", "duplicate proof ID");
    const producer = capturedBasis(
      proof,
      loaded,
      loaded.semanticProofs.get(proof.id),
    );
    if (producer.kind === "semantic") {
      const raw = semanticProofsById.get(proof.id);
      if (
        !raw ||
        !equal(
          (({ freshness, ...captured }) => captured)(raw),
          (({ freshness, ...captured }) => captured)(proof),
        )
      )
        fail("FRESHNESS.BASIS", "provenance", "not an admitted semantic proof");
    }
    if (proof.freshness !== freshness(proof, loaded, producer))
      fail("FRESHNESS.STATE", "freshness", "incorrect freshness label");
    checkedProofs.set(proof.id, proof);
  }
  const normalizedSemantic = records.provenance.filter(
    (proof) => proof.basis !== null,
  );
  if (
    normalizedSemantic.length !== semanticProofsById.size ||
    normalizedSemantic.some((proof) => {
      const captured = semanticProofsById.get(proof.id);
      return (
        !captured ||
        !equal(
          (({ freshness, ...rest }) => rest)(captured),
          (({ freshness, ...rest }) => rest)(proof),
        )
      );
    })
  )
    fail(
      "FRESHNESS.BASIS",
      "provenance",
      "normalized semantic proof inventory differs from captured proofs",
    );
  // A selected coverage tuple can authorize only facts in roles it actually observed.
  // In particular, a failed refresh cannot contain a current call fact, even if
  // the graph traversal would otherwise hide that binding as missing evidence.
  for (const annotation of loaded.annotations)
    for (const fact of annotation.facts) {
      if (
        ![
          "symbol",
          "declarationBinding",
          "typeRelationship",
          "reference",
          "callBinding",
        ].includes(fact.kind)
      )
        continue;
      const proofId =
        fact.kind === "typeRelationship"
          ? fact.provenanceRef
          : fact.record.provenanceId;
      const proof = checkedProofs.get(proofId);
      if (
        !proof ||
        !equal(proof.document, annotation.document) ||
        proof.revisionId !== annotation.revisionId
      )
        fail(
          "COVERAGE.FACT",
          "provenanceId",
          "semantic fact has no captured proof in its document and revision",
        );
      const row = coverageByTuple.get(
        key(proof.producerId, annotation.document, annotation.revisionId),
      );
      if (!row?.selected || !["complete", "partial"].includes(row.state))
        fail(
          "COVERAGE.FACT",
          "coverage",
          "semantic fact cannot exist in an unselected or failed tuple",
        );
      // Only installed occurrence facts claim observed roles. A non-exact join is
      // diagnostic evidence, not a binding/reference applied to measured syntax.
      // Do not guess roles for symbols, declaration bindings or relationships.
      const installed =
        fact.kind === "callBinding"
          ? records.callBindings.some(
              (binding) =>
                binding.provenanceId === proofId &&
                binding.callId !== null &&
                binding.join.status === "exact",
            )
          : fact.kind === "reference" &&
            records.references.some(
              (reference) => reference.provenanceId === proofId,
            );
      const roles = !installed
        ? []
        : fact.kind === "callBinding"
          ? ["call"]
          : fact.record.roles;
      for (const role of roles)
        if (!row.observedRoles.includes(role))
          fail(
            "COVERAGE.FACT",
            "coverage.observedRoles",
            `${fact.kind} requires observed ${role} role`,
          );
    }
  function checkUse({ producerId, document, revisionId, provenanceIds }) {
    const id = key(producerId, document, revisionId),
      coverage = coverageByTuple.get(id);
    if (!coverage)
      fail(
        "FRESHNESS.USE",
        "coverage",
        "missing requested producer-specific tuple",
      );
    if (
      !Array.isArray(provenanceIds) ||
      new Set(provenanceIds).size !== provenanceIds.length
    )
      fail("FRESHNESS.USE", "provenanceIds", "proof IDs repeated or invalid");
    const proofs = provenanceIds.map((proofId) => {
      const proof = checkedProofs.get(proofId);
      if (
        !proof ||
        proof.producerId !== producerId ||
        !equal(proof.document, document) ||
        proof.revisionId !== revisionId ||
        (!semanticProofsById.has(proofId) &&
          expectedProducers.get(producerId)?.kind === "semantic")
      )
        fail(
          "FRESHNESS.USE",
          "provenanceIds",
          "proof outside producer and captured tuple",
        );
      return proof;
    });
    if (
      proofs.length &&
      expectedProducers.get(producerId)?.kind === "semantic" &&
      !coverage.selected
    )
      fail(
        "FRESHNESS.USE",
        "coverage",
        "semantic proof requires selected producer-specific tuple",
      );
    return { coverage, proofs, freshness: proofs.map((x) => x.freshness) };
  }
  function checkTarget(binding, proof, verifiedDeclarations) {
    validate("CallBinding", binding);
    if (binding.provenanceId !== proof?.id || !checkedProofs.has(proof.id))
      fail("FRESHNESS.TARGET", "provenanceId", "binding proof is not checked");
    const target = binding.declaredTarget;
    let staleTarget = null;
    if (target?.kind === "internal") {
      if (!(verifiedDeclarations instanceof Map))
        fail(
          "FRESHNESS.TARGET",
          "declaredTarget",
          "verified declaration map required",
        );
      if (target.revisionId !== proof.revisionId)
        fail(
          "FRESHNESS.TARGET",
          "declaredTarget",
          "internal target must share its binding's revision",
        );
      const captured = documentAt(loaded, target.document, target.revisionId);
      const declaration = verifiedDeclarations
        .get(snapshotKey(target.document.sourceSetId, target.revisionId))
        ?.get(target.syntaxId);
      if (!captured || !declaration || !equal(declaration, target.document))
        fail(
          "FRESHNESS.TARGET",
          "declaredTarget",
          "target not present in captured declarations",
        );
      // v1: a present target at the binding's revision is never stale;
      // positive staleTarget is reserved for later overlays (#11).
      staleTarget = false;
    }
    if (binding.staleTarget !== staleTarget)
      fail(
        "FRESHNESS.TARGET",
        "staleTarget",
        "target bytes or declaration disagree",
      );
    return staleTarget;
  }
  return {
    comparison: records.comparison,
    producers: actualProducers,
    sourceSets: actualSets,
    revisions: actualRevisions,
    coverageByTuple,
    semanticProofsById,
    checkUse,
    checkTarget,
  };
}
