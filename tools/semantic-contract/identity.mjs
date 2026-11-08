import { createHash } from "node:crypto";
import { canonicalBytes } from "./json.mjs";
import { validate } from "./formats.mjs";
import { inventoryMismatch } from "./native-inventory.mjs";

const domains = Object.freeze({
  syntax: "trellis.syntax.v1\0",
  occurrence: "trellis.occurrence.v2\0",
  extractionContext: "trellis.extraction-context.v1\0",
  header: "trellis.header.v1\0",
  siblingGroup: "trellis.sibling-group.v1\0",
});
const shapes = Object.freeze({
  syntax: "SyntaxDigestInput",
  occurrence: "OccurrenceDigestInput",
  extractionContext: "ExtractionContextInput",
  header: "HeaderDigestInput",
  siblingGroup: "SiblingGroupInput",
});
export function digest(kind, input) {
  if (!Object.hasOwn(domains, kind))
    throw new TypeError(`IDENTITY.DOMAIN unknown ${kind}`);
  validate(shapes[kind], input);
  if (kind === "extractionContext") checkComponents(input.components);
  return createHash("sha256")
    .update(domains[kind])
    .update(canonicalBytes(input))
    .digest("hex");
}
export const contentHash = (bytes) =>
  createHash("sha256").update(bytes).digest("hex");
export function sourceManifestHash(rows) {
  validate("SourceManifestInput", rows);
  const seen = new Set();
  let previous = null;
  const languages = ["java", "rust", "python", "javascript", "typescript"];
  for (const row of rows) {
    const key = row.document;
    const identity = canonicalBytes(key).toString("hex");
    if (seen.has(identity))
      throw new Error("IDENTITY.MANIFEST duplicate document");
    seen.add(identity);
    const tuple = [languages.indexOf(key.language), Buffer.from(key.path)];
    if (
      previous &&
      (previous[0] > tuple[0] ||
        (previous[0] === tuple[0] &&
          Buffer.compare(previous[1], tuple[1]) >= 0))
    )
      throw new Error("IDENTITY.MANIFEST unsorted documents");
    previous = tuple;
  }
  return contentHash(canonicalBytes(rows));
}
export const syntaxId = (input) =>
  `sid:v1:${digest("syntax", input).slice(0, 32)}`;
export const occurrenceId = (input) =>
  `occ:v2:${digest("occurrence", input).slice(0, 32)}`;
// Decision 0003: components sort by name then hash (byte order) and are unique.
function checkComponents(components) {
  for (let i = 1; i < components.length; i++) {
    const a = components[i - 1],
      b = components[i];
    const order =
      Buffer.compare(Buffer.from(a.name), Buffer.from(b.name)) ||
      Buffer.compare(Buffer.from(a.hash), Buffer.from(b.hash));
    if (order >= 0)
      throw new Error(
        "IDENTITY.EXTRACTION_CONTEXT components unsorted or duplicate",
      );
  }
}
export const extractionContext = (input) => digest("extractionContext", input);
const contextHashFields = Object.freeze({
  config: "configHash",
  dependency: "dependencyHash",
  toolchain: "toolchainHash",
});
// The native producer's declared inventory is authenticated before any
// occurrence ID is minted: it must equal the trusted inventory of the admitted
// producer version for the document's language, and every component must
// resolve to the revision's captured digest of that kind, backed by an
// authenticated capture.
// An undeclared, unknown, unsorted, duplicate or unbacked component fails closed.
export function nativeOccurrenceContext(fixture, native, revision, document) {
  const fail = (message) => {
    const e = new Error(
      `IDENTITY.EXTRACTION_CONTEXT extractionInputs: ${message}`,
    );
    Object.assign(e, {
      assertion: "IDENTITY.EXTRACTION_CONTEXT",
      code: "invalidRecord",
      field: "extractionInputs",
    });
    throw e;
  };
  const inputs = native?.extractionInputs;
  if (!Array.isArray(inputs)) fail("native inventory not declared");
  const producer = fixture.producers.find(
    (x) => x.id === native.producerId && x.kind === "native",
  );
  if (!producer) fail("native producer descriptor not admitted");
  const mismatch = inventoryMismatch(
    producer.id,
    producer.version,
    document.key.language,
    inputs,
  );
  if (mismatch) fail(mismatch);
  const components = inputs.map((name, i) => {
    if (!Object.hasOwn(contextHashFields, name))
      fail(`undeclared component ${name}`);
    if (i && Buffer.compare(Buffer.from(inputs[i - 1]), Buffer.from(name)) >= 0)
      fail("components unsorted or duplicate");
    const hash = revision?.[contextHashFields[name]];
    if (!fixture.captures.some((x) => x.kind === name && x.hash === hash))
      fail(`component ${name} lacks an authenticated capture`);
    return { name, hash };
  });
  return {
    contentHash: document.contentHash,
    extractionContext: extractionContext({
      language: document.key.language,
      components,
    }),
    nativeProducerId: producer.id,
    nativeProducerVersion: producer.version,
  };
}
export const headerHash = (header) => digest("header", header);
export const siblingGroupHash = (headers) =>
  digest("siblingGroup", { headers });
// Keep one registry per producer's retained identity domain, across revisions.
export function identityRegistry(hash = digest) {
  const seen = new Map();
  function register(kind, input) {
    if (kind !== "syntax" && kind !== "occurrence")
      throw new TypeError("IDENTITY.DOMAIN expected syntax or occurrence");
    validate(shapes[kind], input);
    const value = hash(kind, input);
    if (!/^[0-9a-f]{64}$/.test(value))
      throw new TypeError("IDENTITY.HASH expected full lowercase SHA-256");
    const id = `${kind === "syntax" ? "sid:v1" : "occ:v2"}:${value.slice(0, 32)}`;
    const canonical = canonicalBytes(input).toString("hex");
    const old = seen.get(id);
    if (old !== undefined && old !== canonical)
      throw new Error(`IDENTITY.COLLISION ${id}`);
    seen.set(id, canonical);
    return id;
  }
  return { register };
}
// Each row describes a native declaration in one immutable document snapshot:
// {sourceSetId, language: Language, documentPath, revisionId,
// container: Key[] (outermost to immediate parent, excluding the document
// module), kind, name, signature,
// range:{start,end}, nativeId?}. Native IDs and lookup keys are not identity.
export function assignOrdinals(rows) {
  const groups = new Map();
  for (const row of rows) {
    if (
      typeof row.sourceSetId !== "string" ||
      !row.sourceSetId ||
      typeof row.documentPath !== "string" ||
      !row.documentPath ||
      typeof row.revisionId !== "string" ||
      !row.revisionId ||
      !Array.isArray(row.container) ||
      !row.range ||
      !Number.isSafeInteger(row.range.start) ||
      !Number.isSafeInteger(row.range.end) ||
      row.range.start < 0 ||
      row.range.end < row.range.start
    )
      throw new TypeError("IDENTITY.ORDINAL invalid snapshot declaration row");
    validate("Language", row.language);
    const group = canonicalBytes([
      row.sourceSetId,
      row.language,
      row.documentPath,
      row.revisionId,
      row.container,
      row.kind,
      row.name,
      row.signature,
    ]).toString("hex");
    if (!groups.has(group)) groups.set(group, []);
    groups.get(group).push(row);
  }
  const result = new Map();
  for (const group of groups.values()) {
    group.sort(
      (a, b) => a.range.start - b.range.start || a.range.end - b.range.end,
    );
    for (let i = 0; i < group.length; i++) {
      if (
        i &&
        group[i].range.start === group[i - 1].range.start &&
        group[i].range.end === group[i - 1].range.end
      )
        throw new Error("IDENTITY.ORDINAL duplicate native range");
      result.set(group[i], i);
    }
  }
  return result;
}

export function assignOccurrenceOrdinals(rows) {
  const groups = new Map(),
    result = new Map();
  for (const row of rows) {
    if (!["call", "reference", "control"].includes(row.kind))
      throw new TypeError("IDENTITY.OCCURRENCE_KIND");
    if (
      typeof row.revisionId !== "string" ||
      !row.revisionId ||
      typeof row.ownerSyntaxId !== "string" ||
      !/^sid:v1:[0-9a-f]{32}$/.test(row.ownerSyntaxId) ||
      !row.range ||
      !Number.isSafeInteger(row.range.start) ||
      !Number.isSafeInteger(row.range.end) ||
      row.range.start < 0 ||
      row.range.end <= row.range.start
    )
      throw new TypeError("IDENTITY.OCCURRENCE invalid measured row");
    const key = canonicalBytes([
      row.revisionId,
      row.ownerSyntaxId,
      row.kind,
    ]).toString("hex");
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(row);
  }
  for (const group of groups.values()) {
    group.sort(
      (a, b) => a.range.start - b.range.start || a.range.end - b.range.end,
    );
    for (let i = 0; i < group.length; i++) {
      if (
        i &&
        group[i].range.start === group[i - 1].range.start &&
        group[i].range.end === group[i - 1].range.end
      )
        throw new Error("IDENTITY.OCCURRENCE duplicate native range");
      result.set(group[i], i);
    }
  }
  return result;
}
