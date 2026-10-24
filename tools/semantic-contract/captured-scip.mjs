import { readFile, realpath } from "node:fs/promises";
import { createHash } from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";
import { validate } from "./formats.mjs";
import { parseJson, canonicalBytes } from "./json.mjs";
import { confinedFileCapped } from "./load.mjs";
import { contentHash, sourceManifestHash } from "./identity.mjs";

const profilePath = fileURLToPath(new URL("../../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json", import.meta.url));
const languageOrder = ["java", "rust", "python", "javascript"];
const producers = { "scip-java": ["java", 2], "scip-typescript": ["javascript", 2], "scip-python": ["python", 2], "rust-analyzer": ["rust", 1] };
// Exact named SymbolInformation.Kind values in locked scip 0.10.0 (83 is absent).
const symbolKinds = new Set([...Array(83).keys(), 84, 85, 86]);
const limits = { manifest: 1024 * 1024, artifact: 64 * 1024 * 1024, source: 256 * 1024 * 1024, documents: 1000, candidates: 100000, relationships: 20000 };
function fail(code, message) { const error = new Error(message); error.code = code; throw error; }
function requireThat(condition, message) { if (!condition) fail("InvalidAdmission", message); }
const order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const docOrder = (a, b) => languageOrder.indexOf(a.language) - languageOrder.indexOf(b.language) || order(a.relativePath, b.relativePath);
function sortedUnique(rows) {
  for (let i = 1; i < rows.length; i++) requireThat(docOrder(rows[i - 1], rows[i]) < 0, "unsorted or duplicate documents");
}
function safe(value) { if (!Number.isInteger(value) || value < 0 || value > 2147483647) fail("InvalidArtifact", "coordinate outside i32"); }
function rangeBounds(range) {
  if (range.encoding === "legacy") {
    if (![3, 4].includes(range.values.length)) fail("InvalidArtifact", "legacy range length");
    range.values.forEach(safe);
    return range.values.length === 3 ? [[range.values[0], range.values[1]], [range.values[0], range.values[2]]] : [[range.values[0], range.values[1]], [range.values[2], range.values[3]]];
  }
  for (const value of Object.values(range)) if (typeof value === "number") safe(value);
  return range.encoding === "singleLine" ? [[range.line, range.startCharacter], [range.line, range.endCharacter]] : [[range.startLine, range.startCharacter], [range.endLine, range.endCharacter]];
}
function validateRange(range, text, encoding) {
  const [[sl, sc], [el, ec]] = rangeBounds(range);
  if (sl > el || (sl === el && sc > ec)) fail("InvalidArtifact", "reversed range");
  const lines = text.split(/\r\n|\n|\r/);
  for (const [line, column] of [[sl, sc], [el, ec]]) {
    if (line >= lines.length) fail("InvalidArtifact", "line outside source");
    let offset = 0;
    const boundaries = new Set([0]);
    for (const char of lines[line]) {
      offset += encoding === 1 ? Buffer.byteLength(char) : encoding === 2 ? char.length : 1;
      boundaries.add(offset);
    }
    if (!boundaries.has(column)) fail("InvalidArtifact", "column outside scalar boundary");
  }
}
function digest(domain, value) { return createHash("sha256").update(domain).update(canonicalBytes(value)).digest("hex"); }
async function readInput(root, path, limit) {
  try { return await confinedFileCapped(root, path, limit); }
  catch (error) {
    if (error.code === "LimitExceeded") throw error;
    fail("InvalidAdmission", `${path}: ${error.message}`);
  }
}
function parse(bytes) { try { return parseJson(bytes, { maxDepth: 64 }); } catch (error) { if (error instanceof SyntaxError || error instanceof TypeError) fail("InvalidArtifact", error.message); throw error; } }
function shape(name, value) { try { validate(name, value); } catch (error) { fail("InvalidArtifact", error.message); } }

// Test-only synthetic profile seam; production CLI calls checkCapturedScip without options.
export async function checkCapturedScip(root, expectedManifestHash) {
  return checkWithProfile(root, expectedManifestHash, null);
}
export async function checkCapturedScipWithTestProfile(root, expectedManifestHash, profile) {
  return checkWithProfile(root, expectedManifestHash, profile);
}
async function checkWithProfile(root, expectedManifestHash, profile) {
  requireThat(typeof expectedManifestHash === "string" && /^[a-f0-9]{64}$/.test(expectedManifestHash), "independent manifest hash required");
  const physical = await realpath(root);
  const manifestBytes = await readInput(physical, "captured-scip/manifest.json", limits.manifest);
  requireThat(contentHash(manifestBytes) === expectedManifestHash, "manifest digest mismatch");
  const manifest = parse(manifestBytes);
  shape("ScipManifest", manifest);
  if (manifest.sourceDocuments.length > 100000 || manifest.documents.length > limits.documents) fail("LimitExceeded", "manifest document count");
  let actualProfile;
  try { actualProfile = profile ?? JSON.parse(await readFile(profilePath, "utf8")); }
  catch (error) { fail("InvalidAdmission", `profile unavailable: ${error.message}`); }
  const pinned = actualProfile?.semanticProducers?.find((p) => p.name === manifest.producer.name);
  const [language, productionEncoding] = producers[manifest.producer.name] ?? [];
  const encoding = profile ? profile.expectedPositionEncoding : productionEncoding;
  requireThat(actualProfile.profileId === manifest.profileId && pinned?.version === manifest.producer.version && pinned.executableHash === manifest.producer.executableHash && encoding === manifest.producer.positionEncoding && (profile === null || profile.expectedProducer === manifest.producer.name), "producer profile mismatch");
  requireThat(manifest.sourceSet.id === `source-set:v1:${manifest.sourceSet.rootId}` && manifest.sourceSet.rootId.length > 0 && /^revision:v2:[a-f0-9]{64}$/.test(manifest.sourceRevision), "source identity mismatch");
  requireThat(JSON.stringify(manifest.sourceSet.languages) === JSON.stringify(languageOrder) && manifest.sourceSet.dependencies.length === 0, "source set mismatch");
  requireThat(Object.values(manifest.components).every((hash) => /^[a-f0-9]{64}$/.test(hash)), "component hashes invalid");
  requireThat(manifest.metadata.projectRoot === pathToFileURL(physical).href, "project root mismatch");
  requireThat(manifest.artifacts.length === 1 && manifest.artifacts[0].language === language && manifest.artifacts[0].path.startsWith("captured-scip/") && manifest.artifacts[0].path !== "captured-scip/manifest.json", "artifact selection mismatch");
  const artifactRow = manifest.artifacts[0];
  requireThat(manifest.sourceDocuments.length > 0 && manifest.documents.length > 0, "document count");
  requireThat(manifest.exclusions.length <= manifest.sourceDocuments.length, "exclusion count");
  sortedUnique(manifest.sourceDocuments); sortedUnique(manifest.documents); sortedUnique(manifest.exclusions);
  const sourceMap = new Map();
  let total = 0;
  for (const row of manifest.sourceDocuments) {
    const bytes = await readInput(physical, `sources/${row.relativePath}`, limits.source);
    total += bytes.length;
    if (total > limits.source) fail("LimitExceeded", "total source bytes");
    requireThat(contentHash(bytes) === row.contentHash, "source hash mismatch");
    let text;
    try { text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
    catch { fail("InvalidArtifact", "invalid source UTF-8"); }
    sourceMap.set(row.relativePath, { row, text });
  }
  const rows = manifest.sourceDocuments.map((row) => ({ document: { sourceSetId: manifest.sourceSet.id, language: row.language, path: row.relativePath }, contentHash: row.contentHash }));
  requireThat(sourceManifestHash(rows) === manifest.sourceManifestHash, "source manifest mismatch");
  requireThat(new Set(manifest.sourceDocuments.map((row) => row.relativePath)).size === manifest.sourceDocuments.length, "duplicate source path");
  const covered = new Set();
  for (const row of manifest.documents) {
    requireThat(row.language === language && sourceMap.get(row.relativePath)?.row.language === row.language && sourceMap.get(row.relativePath)?.row.contentHash === row.contentHash && !covered.has(row.relativePath), "document coverage mismatch");
    covered.add(row.relativePath);
  }
  for (const row of manifest.exclusions) {
    requireThat(row.language === language && sourceMap.get(row.relativePath)?.row.language === language && !covered.has(row.relativePath), "exclusion mismatch");
    covered.add(row.relativePath);
  }
  requireThat((manifest.completion === "complete") === (manifest.exclusions.length === 0), "completion mismatch");
  requireThat(manifest.sourceDocuments.filter((row) => row.language === language).every((row) => covered.has(row.relativePath)) && covered.size === manifest.sourceDocuments.filter((row) => row.language === language).length, "incomplete coverage");
  requireThat(JSON.stringify(artifactRow.documents) === JSON.stringify(manifest.documents.map((row) => row.relativePath)), "artifact documents mismatch");
  const artifactBytes = await readInput(physical, artifactRow.path, limits.artifact);
  const artifactHash = contentHash(artifactBytes);
  requireThat(artifactHash === artifactRow.hash, "artifact hash mismatch");
  const artifact = parse(artifactBytes);
  if (!Array.isArray(artifact.documents) || artifact.documents.length > limits.documents) fail("LimitExceeded", "artifact document count");
  for (const document of artifact.documents) {
    if (!Array.isArray(document.facts) || document.facts.length > limits.candidates) fail("LimitExceeded", "candidate fact count");
  }
  shape("ScipFixture", artifact);
  requireThat(artifact.documents.length === manifest.documents.length && artifact.documents.length <= limits.documents, "artifact document count mismatch");
  const claims = [], seen = new Set();
  let candidateCount = 0, relationships = 0;
  for (let i = 0; i < artifact.documents.length; i++) {
    const doc = artifact.documents[i], expected = manifest.documents[i];
    requireThat(doc.language === expected.language && doc.relativePath === expected.relativePath, "artifact document mismatch");
    requireThat(doc.positionEncoding === 0 || doc.positionEncoding === encoding, "position encoding mismatch");
    const counts = { occurrence: 0, symbolInformation: 0 };
    for (const fact of doc.facts) {
      if (++candidateCount > limits.candidates) fail("LimitExceeded", "candidate fact count");
      if (fact.recordKind === "relationship") {
        if (++relationships > limits.relationships) fail("LimitExceeded", "relationship count");
      } else if (++counts[fact.recordKind] > 10000) fail("LimitExceeded", "document fact count");
      if (fact.recordKind === "occurrence") {
        if (fact.raw.symbolRoles > 127 || fact.raw.syntaxKind > 36) fail("InvalidArtifact", "occurrence enum");
        validateRange(fact.raw.range, sourceMap.get(doc.relativePath).text, encoding);
        if (fact.raw.enclosingRange !== null) validateRange(fact.raw.enclosingRange, sourceMap.get(doc.relativePath).text, encoding);
      } else if (fact.recordKind === "symbolInformation") {
        if (!symbolKinds.has(fact.raw.kind)) fail("InvalidArtifact", "symbol kind enum");
      }
      const envelope = { formatVersion: 1, recordKind: fact.recordKind, relativePath: doc.relativePath, positionEncoding: doc.positionEncoding, raw: fact.raw };
      const canonical = canonicalBytes(envelope);
      const identity = `${doc.language}:${canonical.toString("hex")}`;
      if (seen.has(identity)) continue;
      seen.add(identity);
      const factDigest = digest("trellis.semantic-fact.v1\0", envelope);
      const capturedProofId = digest("trellis.capture-proof.v1\0", { producerId: manifest.producer.name, artifactHash, sourceSetId: manifest.sourceSet.id, sourceRevision: manifest.sourceRevision, language: doc.language, relativePath: doc.relativePath, factDigest });
      claims.push({ language: doc.language, envelope, canonicalBytes: canonical, factDigest, capturedProofId });
    }
  }
  claims.sort((a, b) => languageOrder.indexOf(a.language) - languageOrder.indexOf(b.language) || order(a.envelope.relativePath, b.envelope.relativePath) || Buffer.compare(a.canonicalBytes, b.canonicalBytes));
  return { artifactHash, decoderVersion: manifest.decoderVersion, sourceManifestHash: manifest.sourceManifestHash, completion: manifest.completion, exclusions: manifest.exclusions, claims, lookup(proofId, kind, factDigest) {
    const matches = claims.filter((claim) => claim.capturedProofId === proofId && claim.envelope.recordKind === kind && claim.factDigest === factDigest);
    if (!matches.length) fail("MissingProof", "proof absent");
    if (matches.length !== 1) fail("NonUniqueProof", "proof ambiguous");
    return matches[0];
  } };
}
