import { validate } from "./formats.mjs";
import { contentHash } from "./identity.mjs";
import { canonicalBytes } from "./json.mjs";

const tuple = (sourceSetId, revisionId) =>
  JSON.stringify([sourceSetId, revisionId]);
const sameDocument = (a, b) =>
  a.sourceSetId === b.sourceSetId &&
  a.language === b.language &&
  a.path === b.path;
function assert(condition, id, field, message) {
  if (condition) return;
  const error = new Error(`${id} ${field}: ${message}`);
  error.assertion = id;
  error.code = "invalidRecord";
  error.field = field;
  throw error;
}
function capturedDocument(provenance, loaded) {
  const { sourceSetId, path } = provenance.document;
  const revision = loaded.revisions.get(
    tuple(sourceSetId, provenance.revisionId),
  );
  const document = revision?.documents.find((x) =>
    sameDocument(x.key, provenance.document),
  );
  assert(
    document,
    "BASIS.DOCUMENT",
    "document",
    "captured document not admitted",
  );
  assert(
    document.contentHash === provenance.contentHash,
    "BASIS.CONTENT_HASH",
    "contentHash",
    `captured content differs: expected ${document.contentHash}, actual ${provenance.contentHash}`,
  );
  return { revision, document };
}
// Actual digests of the captured bytes of one kind, for mismatch diagnostics.
function capturedHashes(loaded, kind) {
  return loaded.fixture.captures
    .filter((x) => x.kind === kind && loaded.captureBytes.has(x.ref))
    .map((x) => contentHash(loaded.captureBytes.get(x.ref)));
}
const oneOf = (hashes, actual) =>
  `expected one of [${hashes.join(", ")}], actual ${actual}`;
function captureExists(loaded, kind, hash) {
  return loaded.fixture.captures.some(
    (x) =>
      x.kind === kind &&
      x.hash === hash &&
      // A pinned capture was verified against its profile at load (Decision 0017).
      (x.pinned !== undefined ||
        contentHash(loaded.captureBytes.get(x.ref)) === hash),
  );
}

// Check captured claims before examining any requested snapshot or deciding freshness.
export function checkCapturedBasis(provenance, loaded) {
  validate("Provenance", provenance);
  const { revision } = capturedDocument(provenance, loaded);
  const producer = loaded.fixture.producers.find(
    (x) => x.id === provenance.producerId,
  );
  assert(
    producer,
    "BASIS.PRODUCER",
    "producerId",
    "captured producer not admitted",
  );
  assert(
    captureExists(loaded, "executable", producer.executableHash),
    "BASIS.EXECUTABLE",
    "producerHash",
    `missing or changed executable bytes: ${oneOf(capturedHashes(loaded, "executable"), producer.executableHash)}`,
  );
  if (producer.kind === "native") {
    assert(
      provenance.evidenceKind === "measuredSyntax" && provenance.basis === null,
      "BASIS.NATIVE_PAIRING",
      "basis",
      "native syntax requires null semantic basis",
    );
    return { producer, revision };
  }
  assert(
    provenance.evidenceKind !== "measuredSyntax" && provenance.basis !== null,
    "BASIS.SEMANTIC_PAIRING",
    "basis",
    "semantic evidence requires basis",
  );
  const basis = provenance.basis;
  const checks = {
    producerId: producer.id,
    producerVersion: producer.version,
    producerHash: producer.executableHash,
    language: provenance.document.language,
    sourceSetId: provenance.document.sourceSetId,
    revisionId: provenance.revisionId,
    sourceManifestHash: loaded.sourceManifestHash(revision),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
  };
  for (const [field, expected] of Object.entries(checks))
    assert(
      basis[field] === expected,
      `BASIS.${field.toUpperCase()}`,
      field,
      `captured claim differs: expected ${expected}, actual ${basis[field]}`,
    );
  assert(
    producer.languages.includes(basis.language),
    "BASIS.LANGUAGE",
    "language",
    "producer does not support captured language",
  );
  for (const [field, kind] of [
    ["toolchainHash", "toolchain"],
    ["configHash", "config"],
    ["dependencyHash", "dependency"],
  ])
    assert(
      captureExists(loaded, kind, basis[field]),
      `BASIS.${field.toUpperCase()}`,
      field,
      `captured bytes unavailable: ${oneOf(capturedHashes(loaded, kind), basis[field])}`,
    );
  assert(
    loaded.semanticBytes.some(
      ({ bytes, value }) =>
        value.producerId === producer.id &&
        contentHash(bytes) === basis.artifactHash,
    ),
    "BASIS.ARTIFACTHASH",
    "artifactHash",
    `semantic artifact bytes differ: ${oneOf(
      loaded.semanticBytes
        .filter(({ value }) => value.producerId === producer.id)
        .map(({ bytes }) => contentHash(bytes)),
      basis.artifactHash,
    )}`,
  );
  const deps = basis.lookupDependencies;
  assert(
    deps.every((x) => !/^(?:sid|occ):v1:[a-f0-9]{32}$/.test(x)),
    "BASIS.LOOKUP_DEPENDENCIES",
    "lookupDependencies",
    "lookup keys cannot be stable IDs",
  );
  for (let i = 1; i < deps.length; i++)
    assert(
      Buffer.compare(Buffer.from(deps[i - 1]), Buffer.from(deps[i])) < 0,
      "BASIS.LOOKUP_DEPENDENCIES",
      "lookupDependencies",
      "keys must be sorted and unique",
    );
  const proof = loaded.semanticProofs?.get(provenance.id);
  const withoutFreshness = ({ freshness, ...captured }) => captured;
  assert(
    proof?.hash === basis.artifactHash &&
      canonicalBytes(withoutFreshness(proof.wrapper)).equals(
        canonicalBytes(withoutFreshness(provenance)),
      ),
    "BASIS.RAW_FACT",
    "provenance",
    "no matching captured semantic fact and original proof wrapper",
  );
  return { producer, revision };
}

// The requested snapshot defaults to the fixture comparison. A graph request may
// pin another admitted revision; it then compares against captured producers.
function requestedSnapshot(loaded, request) {
  const comparison = loaded.comparison;
  const at = request ?? comparison;
  const matches =
    at.sourceSetId === comparison.sourceSetId &&
    at.revisionId === comparison.revisionId;
  return {
    sourceSetId: at.sourceSetId,
    revision: matches
      ? loaded.selected
      : loaded.revisions.get(tuple(at.sourceSetId, at.revisionId)),
    producers: matches ? comparison.producers : loaded.fixture.producers,
  };
}
export function expectedFreshness(provenance, loaded, request) {
  const { producer } = checkCapturedBasis(provenance, loaded);
  const { sourceSetId, revision, producers } = requestedSnapshot(
    loaded,
    request,
  );
  const wanted = revision?.documents.find(
    (x) =>
      x.key.path === provenance.document.path &&
      x.key.language === provenance.document.language,
  );
  if (!wanted || wanted.contentHash !== provenance.contentHash) return "stale";
  if (producer.kind === "native")
    return revision.id === provenance.revisionId &&
      sourceSetId === provenance.document.sourceSetId
      ? "fresh"
      : "possiblyStale";
  const basis = provenance.basis;
  const requested = producers.find((x) => x.id === basis.producerId);
  if (
    !requested ||
    requested.kind !== "semantic" ||
    !requested.languages.includes(basis.language) ||
    requested.version !== basis.producerVersion ||
    requested.executableHash !== basis.producerHash ||
    requested.positionEncoding !== producer.positionEncoding ||
    JSON.stringify(requested.languages) !==
      JSON.stringify(producer.languages) ||
    !captureExists(loaded, "executable", requested.executableHash) ||
    sourceSetId !== basis.sourceSetId ||
    revision.id !== basis.revisionId ||
    loaded.sourceManifestHash(revision) !== basis.sourceManifestHash ||
    revision.toolchainHash !== basis.toolchainHash ||
    revision.configHash !== basis.configHash ||
    revision.dependencyHash !== basis.dependencyHash
  )
    return "possiblyStale";
  return "fresh";
}
export function checkFreshness(provenance, loaded) {
  const expected = expectedFreshness(provenance, loaded);
  assert(
    provenance.freshness === expected,
    "FRESHNESS.LABEL",
    "freshness",
    `expected ${expected}`,
  );
  return expected;
}

// Declaration presence is supplied by the verified normalization layer. Keys are
// logical tuples, not serialized DocumentKey objects or object insertion order.
export function expectedStaleTarget(binding, provenance, loaded, declarations) {
  validate("CallBinding", binding);
  checkCapturedBasis(provenance, loaded);
  assert(
    binding.provenanceId === provenance.id,
    "TARGET_STALENESS.PROVENANCE",
    "provenanceId",
    "binding proof differs",
  );
  const target = binding.declaredTarget;
  if (!target || target.kind !== "internal") return null;
  assert(
    declarations instanceof Map,
    "TARGET_STALENESS.DECLARATIONS",
    "declarations",
    "verified declaration map required",
  );
  const captured = loaded.revisions
    .get(tuple(target.document.sourceSetId, target.revisionId))
    ?.documents.find((x) => sameDocument(x.key, target.document));
  const capturedDeclaration = declarations
    .get(tuple(target.document.sourceSetId, target.revisionId))
    ?.get(target.syntaxId);
  assert(
    captured &&
      capturedDeclaration &&
      sameDocument(capturedDeclaration, target.document),
    "TARGET_STALENESS.CAPTURED",
    "declaredTarget",
    "target declaration not present in captured snapshot",
  );
  // v1 (#52): an internal target has its binding's revision, where it was just
  // verified present, so it is never stale. Positive staleTarget is reserved
  // for later overlays (#11).
  assert(
    target.revisionId === provenance.revisionId,
    "TARGET_STALENESS.REVISION",
    "declaredTarget",
    "internal target must share its binding's revision",
  );
  return false;
}
export function checkStaleTarget(binding, provenance, loaded, declarations) {
  const expected = expectedStaleTarget(
    binding,
    provenance,
    loaded,
    declarations,
  );
  assert(
    binding.staleTarget === expected,
    "TARGET_STALENESS.LABEL",
    "staleTarget",
    `expected ${expected}`,
  );
  return expected;
}
