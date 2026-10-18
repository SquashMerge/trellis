import { createHash } from "node:crypto";
import { canonicalBytes } from "../json.mjs";
import { validate } from "../formats.mjs";
import { toByteRange } from "../coordinates.mjs";
import { lookupKey } from "../lookup.mjs";
import { inventoryMismatch } from "../native-inventory.mjs";

const bytes = (value) => canonicalBytes(value);
const hex = (value) => bytes(value).toString("hex");
const equal = (a, b) => hex(a) === hex(b);
const ascii = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const cmp = (a, b) => Buffer.compare(bytes(a), bytes(b));
const contains = (outer, inner) =>
  outer.start <= inner.start && inner.end <= outer.end;
const strict = (outer, inner) => contains(outer, inner) && !equal(outer, inner);
const snapshot = (row) =>
  hex([row.document.sourceSetId, row.revisionId, row.document.path]);
function reject(assertion, field, reason, code = "invalidRecord") {
  const error = new Error(`${assertion} ${field}: ${reason}`);
  Object.assign(error, { assertion, field, code });
  throw error;
}
function coordinate(source, range) {
  try {
    return toByteRange(source, range);
  } catch (error) {
    if (error.message?.includes("COORD.INVALID_RANGE"))
      reject("MEASUREMENT.RANGE", "range", error.message, "invalidRange");
    throw error;
  }
}
function sourceFor(loaded, row) {
  const source = loaded.sources.get(
    JSON.stringify([
      row.document.sourceSetId,
      row.revisionId,
      row.document.path,
    ]),
  );
  const revision = loaded.revisions.get(
    JSON.stringify([row.document.sourceSetId, row.revisionId]),
  );
  const document = revision?.documents.find((x) => equal(x.key, row.document));
  if (!source || !document)
    reject("MEASUREMENT.OWNER", "document", "unadmitted document or revision");
  return { source: Buffer.from(source), document };
}
function rangeOf(source, range, encoding) {
  if (range.encoding !== encoding)
    reject("MEASUREMENT.ENCODING", "range", "native producer encoding differs");
  return coordinate(source, range);
}
function leaves(row) {
  const fields = [];
  const add = (field, text, range = null) => {
    if (text !== null) fields.push({ field, text, range });
  };
  if (Object.hasOwn(row, "header")) {
    add("name", row.name, row.nameRange);
    add("header.name", row.header.name, row.nameRange);
    row.header.modifiers.forEach((text, i) =>
      add(`header.modifiers[${i}]`, text),
    );
    row.header.typeParameters.forEach((text, i) =>
      add(`header.typeParameters[${i}]`, text),
    );
    row.header.parameters.forEach((p, i) => {
      add(`header.parameters[${i}].name`, p.name);
      add(`header.parameters[${i}].type`, p.type);
    });
    add("header.resultType", row.header.resultType);
    row.header.bases.forEach((text, i) => add(`header.bases[${i}]`, text));
    row.signature?.parameterTypes.forEach((text, i) =>
      add(`signature.parameterTypes[${i}]`, text),
    );
  } else if (Object.hasOwn(row, "spelling"))
    add(
      "spelling",
      row.spelling,
      row.calleeRange ?? (Object.hasOwn(row, "regionRefs") ? null : row.range),
    );
  return fields;
}
function witnesses(row, source, within, encoding) {
  const expected = new Map(leaves(row).map((x) => [x.field, x]));
  if (expected.size !== leaves(row).length)
    reject("MEASUREMENT.WITNESS", "field", "duplicate leaf");
  const seen = new Set();
  for (const { field, witness } of row.witnesses) {
    const leaf = expected.get(field);
    if (!leaf || seen.has(field))
      reject("MEASUREMENT.WITNESS", "field", "extra or duplicate witness");
    seen.add(field);
    if (witness.range.encoding !== encoding)
      reject(
        "MEASUREMENT.ENCODING",
        "witnesses." + field,
        "witness encoding differs",
      );
    const actual = coordinate(source, witness.range);
    if (
      !contains(within, actual) ||
      witness.text !== leaf.text ||
      source.subarray(actual.start, actual.end).toString("utf8") !==
        leaf.text ||
      (leaf.range !== null &&
        !equal(actual, rangeOf(source, leaf.range, encoding)))
    )
      reject(
        "MEASUREMENT.WITNESS",
        field,
        "witness is not the measured source spelling",
      );
  }
  for (const leaf of expected.values())
    if (!seen.has(leaf.field))
      reject("MEASUREMENT.WITNESS", leaf.field, "missing source witness");
}
// Decision 0003: occurrence identity is v2; every other domain stays v1.
function hash(domain, input) {
  return createHash("sha256")
    .update(`trellis.${domain}.${domain === "occurrence" ? "v2" : "v1"}\0`)
    .update(bytes(input))
    .digest("hex");
}
export const measuredHeaderHash = (header) => hash("header", header);
export const measuredSiblingGroupHash = (headers) =>
  hash("sibling-group", { headers });
// Storage ordering is not traversal order. Do not sort the measured source rows.
export function compareEnvelope(a, b) {
  if (a.capturedRevisionId && b.capturedRevisionId)
    return (
      ascii(a.capturedRevisionId, b.capturedRevisionId) ||
      cmp(a.document, b.document) ||
      cmp(a, b)
    );
  if (a.syntaxId && b.syntaxId)
    return (
      ascii(a.syntaxId, b.syntaxId) ||
      ascii(a.revisionId, b.revisionId) ||
      cmp(a.document, b.document)
    );
  if (a.id && b.id) return ascii(a.id, b.id) || cmp(a, b);
  return cmp(a, b);
}
export function compareEnvelopeField(field, a, b) {
  return field === "referenceJoinDiagnostics"
    ? ascii(a.factRef, b.factRef)
    : compareEnvelope(a, b);
}
export function checkEnvelopeOrder(field, rows) {
  for (let i = 1; i < rows.length; i++)
    if (compareEnvelopeField(field, rows[i - 1], rows[i]) > 0)
      reject(
        "RECORDS.ORDER",
        field,
        "records are not in deterministic storage order",
      );
}
export function orderedEnvelope(
  field,
  rows,
  { collapseIdentical = false } = {},
) {
  if (
    collapseIdentical &&
    [
      "durableAnchors",
      "groupContinuities",
      "anchorResults",
      "declarations",
      "calls",
      "controlRegions",
    ].includes(field)
  )
    reject(
      "RECORDS.MEMBERSHIP",
      field,
      "authored or measured multiplicity must be preserved",
    );
  const values = collapseIdentical
    ? [...new Map(rows.map((row) => [hex(row), row])).values()]
    : [...rows];
  return values.sort((a, b) => compareEnvelopeField(field, a, b));
}
function closure(field, expected, actual) {
  checkEnvelopeOrder(field, actual);
  const ordered = [...expected].sort((a, b) =>
    compareEnvelopeField(field, a, b),
  );
  if (!equal(actual, ordered))
    reject(
      "RECORDS.MEMBERSHIP",
      field,
      "source-derived measured records differ",
    );
}
function spell(language, text) {
  try {
    return lookupKey(language, text);
  } catch (error) {
    reject("MEASUREMENT.WITNESS", "spelling", error.message);
  }
}
export function checkMeasurement(
  loaded,
  records,
  { handleDigest = hash } = {},
) {
  const handles = new Map();
  function handle(domain, input) {
    const value = handleDigest(domain, input);
    if (typeof value !== "string" || !/^[0-9a-f]{64}$/.test(value))
      reject("ID.SOURCE", "id", "digest must be a full lowercase SHA-256 hash");
    const id = `${domain === "syntax" ? "sid:v1" : "occ:v2"}:${value.slice(0, 32)}`;
    const descriptor = hex(input),
      prior = handles.get(id);
    if (prior !== undefined && prior !== descriptor)
      reject("ID.SOURCE", "id", "distinct canonical descriptors collide");
    handles.set(id, descriptor);
    return id;
  }
  validate("NativeArtifact", loaded.native);
  const native = loaded.native;
  const producer = loaded.fixture.producers.find(
    (x) => x.id === native.producerId && x.kind === "native",
  );
  if (!producer)
    reject("MEASUREMENT.OWNER", "producerId", "native producer not admitted");
  const encoding = producer.positionEncoding;
  // Independently authenticate the declared extraction-input inventory before
  // any occurrence ID is re-derived. No inventory, one that differs from the
  // trusted inventory of the admitted producer version and row language, an
  // unknown/unsorted/duplicate component, or one without an authenticated
  // capture fails closed.
  const contextFields = {
    config: "configHash",
    dependency: "dependencyHash",
    toolchain: "toolchainHash",
  };
  const inventory = native.extractionInputs;
  inventory.forEach((name, i) => {
    if (
      !Object.hasOwn(contextFields, name) ||
      (i && ascii(inventory[i - 1], name) >= 0)
    )
      reject(
        "IDENTITY.EXTRACTION_CONTEXT",
        "extractionInputs",
        "undeclared, unsorted or duplicate extraction component",
      );
  });
  function occurrenceContext(row) {
    const revision = loaded.revisions.get(
      JSON.stringify([row.document.sourceSetId, row.revisionId]),
    );
    const { document } = sourceFor(loaded, row);
    const mismatch = inventoryMismatch(
      producer.id,
      producer.version,
      row.document.language,
      inventory,
    );
    if (mismatch)
      reject("IDENTITY.EXTRACTION_CONTEXT", "extractionInputs", mismatch);
    const components = inventory.map((name) => {
      const value = revision[contextFields[name]];
      if (
        !loaded.fixture.captures.some(
          (x) => x.kind === name && x.hash === value,
        )
      )
        reject(
          "IDENTITY.EXTRACTION_CONTEXT",
          "extractionInputs",
          `component ${name} lacks an authenticated capture`,
        );
      return { name, hash: value };
    });
    return {
      contentHash: document.contentHash,
      extractionContext: createHash("sha256")
        .update("trellis.extraction-context.v1\0")
        .update(bytes({ language: row.document.language, components }))
        .digest("hex"),
      nativeProducerId: producer.id,
      nativeProducerVersion: producer.version,
    };
  }
  const all = [
    ...native.declarations,
    ...native.calls,
    ...native.controls,
    ...native.references,
  ];
  const refs = new Set();
  for (const row of all) {
    if (refs.has(row.ref))
      reject("RECORDS.MEMBERSHIP", "ref", "duplicate native reference");
    refs.add(row.ref);
  }
  const declarationByRef = new Map(native.declarations.map((x) => [x.ref, x]));
  const identityByRef = new Map(),
    recordByNativeRef = new Map(),
    position = new Map(),
    declaration = new Map(),
    groupsByDeclarationRef = new Map();
  const ids = new Map(),
    descriptors = new Map(),
    decls = [];
  for (const row of native.declarations) {
    const { source } = sourceFor(loaded, row);
    const range = rangeOf(source, row.range, encoding);
    const nameRange =
      row.nameRange === null ? null : rangeOf(source, row.nameRange, encoding);
    if (range.start === range.end && row.kind !== "module")
      reject(
        "MEASUREMENT.RANGE",
        "range",
        "only a module can have an empty range",
      );
    if (
      (row.name === null) !== (row.nameRange === null) ||
      (row.kind === "module" || row.kind === "anonymousFunction") !==
        (row.name === null) ||
      (nameRange !== null && !contains(range, nameRange))
    )
      reject("MEASUREMENT.WITNESS", "nameRange", "invalid name and range");
    if (row.header.kind !== row.kind || row.header.name !== row.name)
      reject("MEASUREMENT.WITNESS", "header", "header projection differs");
    if (row.header.parameters.slice(0, -1).some((x) => x.variadic))
      reject(
        "MEASUREMENT.WITNESS",
        "header.parameters",
        "variadic parameter must be final",
      );
    if (row.document.language === "java" && row.kind === "function")
      reject(
        "MEASUREMENT.WITNESS",
        "kind",
        "Java ordinary function does not exist",
      );
    if (
      row.signature !== null &&
      (row.document.language !== "java" ||
        !["method", "constructor"].includes(row.kind))
    )
      reject(
        "MEASUREMENT.WITNESS",
        "signature",
        "signature only applies to Java methods/constructors",
      );
    if (
      row.signature !== null &&
      (row.signature.typeParameterCount !== row.header.typeParameters.length ||
        row.signature.parameterTypes.length !== row.header.parameters.length ||
        row.signature.variadic !==
          (row.header.parameters.at(-1)?.variadic ?? false) ||
        row.header.parameters.slice(0, -1).some((x) => x.variadic) ||
        row.signature.parameterTypes.some(
          (x, i) => x !== row.header.parameters[i].type,
        ))
    )
      reject(
        "MEASUREMENT.WITNESS",
        "signature",
        "signature projection differs",
      );
    witnesses(row, source, range, encoding);
    position.set(row.ref, { range, nameRange });
    decls.push(row);
  }
  function ancestors(row, visiting = new Set()) {
    if (descriptors.has(row.ref)) return descriptors.get(row.ref);
    if (visiting.has(row.ref))
      reject("MEASUREMENT.OWNER", "parentRef", "parent cycle");
    visiting.add(row.ref);
    let chain = [];
    const child = position.get(row.ref).range;
    if (row.parentRef !== null) {
      const parent = declarationByRef.get(row.parentRef);
      if (!parent || snapshot(parent) !== snapshot(row))
        reject(
          "MEASUREMENT.OWNER",
          "parentRef",
          "parent missing or in another snapshot",
        );
      const parentAncestors = ancestors(parent, visiting),
        outer = position.get(parent.ref).range;
      if (!contains(outer, child))
        reject(
          "MEASUREMENT.OWNER",
          "parentRef",
          "parent does not contain child",
        );
      chain =
        parent.kind === "module"
          ? [...parentAncestors]
          : [...parentAncestors, parent.ref];
    }
    const candidates = decls.filter(
      (x) =>
        x.ref !== row.ref &&
        snapshot(x) === snapshot(row) &&
        strict(position.get(x.ref).range, child),
    );
    const immediate = candidates.filter(
      (x) =>
        !candidates.some(
          (y) =>
            y !== x &&
            strict(position.get(x.ref).range, position.get(y.ref).range) &&
            contains(position.get(y.ref).range, child),
        ),
    );
    if (immediate.length === 0)
      immediate.push(
        ...decls.filter(
          (x) =>
            x.ref !== row.ref &&
            x.kind === "module" &&
            snapshot(x) === snapshot(row) &&
            equal(position.get(x.ref).range, child),
        ),
      );
    if (
      (row.parentRef === null && immediate.length) ||
      (row.parentRef !== null &&
        (!immediate.some((x) => x.ref === row.parentRef) ||
          immediate.some((x) => x.ref !== row.parentRef)))
    )
      reject(
        "MEASUREMENT.OWNER",
        "parentRef",
        "not the immediate source container",
      );
    descriptors.set(row.ref, chain);
    visiting.delete(row.ref);
    return chain;
  }
  for (const row of decls) ancestors(row);
  // Resolve dependencies by depth: ordinal groups use the exact container keys, not native IDs.
  for (const depth of [
    ...new Set([...descriptors.values()].map((x) => x.length)),
  ].sort((a, b) => a - b)) {
    const at = decls.filter((x) => descriptors.get(x.ref).length === depth);
    const groups = new Map();
    for (const row of at) {
      const ancestry = descriptors.get(row.ref).map((ref) => ids.get(ref)?.key);
      if (ancestry.includes(undefined))
        reject("MEASUREMENT.OWNER", "parentRef", "missing ancestor identity");
      const key = hex([
        snapshot(row),
        ancestry,
        row.kind,
        row.name,
        row.signature,
      ]);
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push({ row, ancestry });
    }
    for (const group of groups.values()) {
      group.sort(
        (a, b) =>
          position.get(a.row.ref).range.start -
            position.get(b.row.ref).range.start ||
          position.get(a.row.ref).range.end - position.get(b.row.ref).range.end,
      );
      for (let i = 0; i < group.length; i++) {
        const { row, ancestry } = group[i],
          range = position.get(row.ref).range;
        if (i && equal(range, position.get(group[i - 1].row.ref).range))
          reject("ID.ORDINAL", "range", "duplicate sibling range");
        const key = {
          kind: row.kind,
          name: row.name,
          signature: row.signature,
          ordinal: i,
        };
        const id = handle("syntax", {
          sourceSet: row.document.sourceSetId,
          path: row.document.path,
          language: row.document.language,
          ancestors: ancestry,
          declaration: key,
        });
        const value = {
          role: row.role,
          syntaxId: id,
          document: row.document,
          revisionId: row.revisionId,
          kind: row.kind,
          name: row.name,
          lookupKey:
            row.name === null ? null : spell(row.document.language, row.name),
          ancestors: ancestry,
          key,
          range,
          nameRange: position.get(row.ref).nameRange,
          header: row.header,
          provenanceId: `native:${row.revisionId}:${id}`,
        };
        validate("Declaration", value);
        ids.set(row.ref, { id, key });
        identityByRef.set(row.ref, id);
        recordByNativeRef.set(row.ref, value);
        declaration.set(
          hex([row.document.sourceSetId, row.revisionId, id]),
          value,
        );
      }
    }
  }
  const declarations = [...recordByNativeRef.values()];
  for (const row of decls) {
    const value = recordByNativeRef.get(row.ref);
    const peers = decls
      .filter(
        (x) =>
          snapshot(x) === snapshot(row) &&
          equal(descriptors.get(x.ref), descriptors.get(row.ref)) &&
          x.kind === row.kind &&
          x.name === row.name &&
          equal(x.signature, row.signature),
      )
      .sort(
        (a, b) =>
          position.get(a.ref).range.start - position.get(b.ref).range.start ||
          position.get(a.ref).range.end - position.get(b.ref).range.end,
      );
    groupsByDeclarationRef.set(row.ref, {
      memberRefs: peers.map((x) => x.ref),
      headers: peers.map((x) => measuredHeaderHash(x.header)),
    });
  }
  const items = [],
    measuredOccurrence = new Map(),
    duplicate = new Set();
  for (const [kind, rows] of [
    ["call", native.calls],
    ["control", native.controls],
    ["reference", native.references],
  ])
    for (const row of rows) {
      const owner = declarationByRef.get(row.ownerRef);
      if (!owner || snapshot(owner) !== snapshot(row))
        reject(
          "MEASUREMENT.OWNER",
          "ownerRef",
          "missing or cross-snapshot owner",
        );
      const { source } = sourceFor(loaded, row),
        range = rangeOf(source, row.range, encoding),
        ownerRange = position.get(owner.ref).range;
      if (range.start === range.end)
        reject("MEASUREMENT.RANGE", "range", "empty occurrence range");
      if (!contains(ownerRange, range))
        reject("MEASUREMENT.OWNER", "range", "occurrence outside owner");
      if (kind === "call" || kind === "control") {
        if (
          owner.kind !== "module" &&
          !["function", "method", "constructor", "anonymousFunction"].includes(
            owner.kind,
          )
        )
          reject(
            "MEASUREMENT.OWNER",
            "ownerRef",
            "call/control requires callable or module owner",
          );
        for (const inner of decls)
          if (
            inner.ref !== owner.ref &&
            snapshot(inner) === snapshot(row) &&
            strict(ownerRange, position.get(inner.ref).range) &&
            contains(position.get(inner.ref).range, range) &&
            ["function", "method", "constructor", "anonymousFunction"].includes(
              inner.kind,
            )
          )
            reject(
              "MEASUREMENT.OWNER",
              "ownerRef",
              "nested callable owns occurrence",
            );
      }
      const dup = hex([row.revisionId, ids.get(owner.ref).id, kind, range]);
      if (duplicate.has(dup))
        reject("ID.ORDINAL", "range", "duplicate measured occurrence");
      duplicate.add(dup);
      let calleeRange = null;
      if (kind === "call" && row.calleeRange !== null) {
        calleeRange = rangeOf(source, row.calleeRange, encoding);
        if (!contains(range, calleeRange))
          reject(
            "MEASUREMENT.RANGE",
            "calleeRange",
            "callee outside invocation",
          );
      }
      witnesses(row, source, range, encoding);
      if (
        kind === "reference" &&
        source.subarray(range.start, range.end).toString("utf8") !==
          row.spelling
      )
        reject(
          "MEASUREMENT.WITNESS",
          "spelling",
          "reference differs from source",
        );
      items.push({
        row,
        kind,
        range,
        calleeRange,
        ownerSyntaxId: ids.get(owner.ref).id,
      });
    }
  const grouped = new Map();
  for (const x of items) {
    const key = hex([x.row.revisionId, x.ownerSyntaxId, x.kind]);
    if (!grouped.has(key)) grouped.set(key, []);
    grouped.get(key).push(x);
  }
  for (const group of grouped.values()) {
    group.sort(
      (a, b) => a.range.start - b.range.start || a.range.end - b.range.end,
    );
    group.forEach((x, i) => {
      x.ordinal = i;
      x.id = handle("occurrence", {
        ...occurrenceContext(x.row),
        ownerSyntaxId: x.ownerSyntaxId,
        kind: x.kind,
        ordinal: i,
      });
      identityByRef.set(x.row.ref, x.id);
      measuredOccurrence.set(hex([x.row.revisionId, x.kind, x.id]), x);
    });
  }
  const controlByRef = new Map(
    items.filter((x) => x.kind === "control").map((x) => [x.row.ref, x]),
  );
  function controlParents(x, visiting = new Set()) {
    if (visiting.has(x.row.ref))
      reject("MEASUREMENT.CONTROL", "parentRef", "control cycle");
    visiting.add(x.row.ref);
    const parent =
      x.row.parentRef === null ? null : controlByRef.get(x.row.parentRef);
    if (
      x.row.parentRef !== null &&
      (!parent ||
        parent.row.ownerRef !== x.row.ownerRef ||
        snapshot(parent.row) !== snapshot(x.row) ||
        !contains(parent.range, x.range))
    )
      reject(
        "MEASUREMENT.CONTROL",
        "parentRef",
        "control parent differs or does not contain child",
      );
    if (parent) controlParents(parent, visiting);
    visiting.delete(x.row.ref);
    const candidates = [...controlByRef.values()].filter(
      (y) =>
        y !== x &&
        y.row.ownerRef === x.row.ownerRef &&
        snapshot(y.row) === snapshot(x.row) &&
        strict(y.range, x.range),
    );
    if (
      (candidates.length && !parent) ||
      (parent &&
        candidates.some(
          (y) =>
            y !== parent &&
            strict(parent.range, y.range) &&
            contains(y.range, x.range),
        ))
    )
      reject(
        "MEASUREMENT.CONTROL",
        "parentRef",
        "not immediate control parent",
      );
  }
  for (const x of controlByRef.values()) controlParents(x);
  const controlRegions = items
    .filter((x) => x.kind === "control")
    .map((x) => {
      const value = {
        id: x.id,
        ownerSyntaxId: x.ownerSyntaxId,
        ordinal: x.ordinal,
        document: x.row.document,
        revisionId: x.row.revisionId,
        kind: x.row.kind,
        range: x.range,
        parentId:
          x.row.parentRef === null ? null : identityByRef.get(x.row.parentRef),
        arm: x.row.arm,
        provenanceId: `native:${x.row.revisionId}:${x.id}`,
      };
      validate("ControlRegion", value);
      recordByNativeRef.set(x.row.ref, value);
      return value;
    });
  const calls = items
    .filter((x) => x.kind === "call")
    .map((x) => {
      let last = null;
      const seen = new Set();
      for (const ref of x.row.regionRefs) {
        const region = controlByRef.get(ref);
        if (
          !region ||
          seen.has(ref) ||
          region.row.ownerRef !== x.row.ownerRef ||
          snapshot(region.row) !== snapshot(x.row) ||
          !contains(region.range, x.range) ||
          region.row.parentRef !== last
        )
          reject(
            "MEASUREMENT.REGION",
            "regionRefs",
            "invalid ordered containment chain",
          );
        seen.add(ref);
        last = ref;
      }
      const containing = [...controlByRef.values()].filter(
        (region) =>
          region.row.ownerRef === x.row.ownerRef &&
          snapshot(region.row) === snapshot(x.row) &&
          contains(region.range, x.range),
      );
      if (containing.length !== seen.size)
        reject(
          "MEASUREMENT.REGION",
          "regionRefs",
          "missing containing control region",
        );
      const value = {
        id: x.id,
        ownerSyntaxId: x.ownerSyntaxId,
        ordinal: x.ordinal,
        document: x.row.document,
        revisionId: x.row.revisionId,
        range: x.range,
        calleeRange: x.calleeRange,
        spelling: x.row.spelling,
        regionIds: x.row.regionRefs.map((ref) => identityByRef.get(ref)),
        provenanceId: `native:${x.row.revisionId}:${x.id}`,
      };
      validate("Call", value);
      recordByNativeRef.set(x.row.ref, value);
      return value;
    });
  const nativeReferenceDescriptors = items
    .filter((x) => x.kind === "reference")
    .map((x) => ({
      ref: x.row.ref,
      id: x.id,
      ownerRef: x.row.ownerRef,
      ownerSyntaxId: x.ownerSyntaxId,
      ordinal: x.ordinal,
      document: x.row.document,
      revisionId: x.row.revisionId,
      range: x.range,
      spelling: x.row.spelling,
      lookupKey: spell(x.row.document.language, x.row.spelling),
    }));
  const candidateRows = [];
  for (const row of decls) {
    const range = position.get(row.ref).nameRange;
    if (range !== null) {
      const { document } = sourceFor(loaded, row);
      candidateRows.push({
        ref: row.ref,
        id: identityByRef.get(row.ref),
        ownerRef: row.parentRef ?? row.ref,
        anchor: {
          document: row.document,
          revisionId: row.revisionId,
          contentHash: document.contentHash,
          range,
          kind: "declarationName",
        },
      });
    }
  }
  for (const x of items.filter((x) => x.kind !== "control")) {
    const { document } = sourceFor(loaded, x.row);
    for (const [kind, range] of x.kind === "call"
      ? [
          ["invocation", x.range],
          ...(x.calleeRange === null ? [] : [["callee", x.calleeRange]]),
        ]
      : [["reference", x.range]])
      candidateRows.push({
        ref: x.row.ref,
        id: x.id,
        ownerRef: x.row.ownerRef,
        anchor: {
          document: x.row.document,
          revisionId: x.row.revisionId,
          contentHash: document.contentHash,
          range,
          kind,
        },
      });
  }
  const nativeProofRows = all.map((row) => {
    const { document } = sourceFor(loaded, row),
      id = identityByRef.get(row.ref);
    const selected =
      loaded.selected ??
      loaded.revisions.get(
        JSON.stringify([row.document.sourceSetId, row.revisionId]),
      );
    const current = selected?.documents.find(
      (x) =>
        x.key.path === row.document.path &&
        x.key.language === row.document.language,
    );
    const freshness =
      !current || current.contentHash !== document.contentHash
        ? "stale"
        : selected.id === row.revisionId &&
            (loaded.comparison?.sourceSetId ?? row.document.sourceSetId) ===
              row.document.sourceSetId
          ? "fresh"
          : "possiblyStale";
    return {
      id: `native:${row.revisionId}:${id}`,
      producerId: native.producerId,
      document: row.document,
      revisionId: row.revisionId,
      contentHash: document.contentHash,
      evidenceKind: "measuredSyntax",
      basis: null,
      freshness,
    };
  });
  closure("declarations", declarations, records.declarations);
  closure("calls", calls, records.calls);
  closure("controlRegions", controlRegions, records.controlRegions);
  return {
    identityByRef,
    recordByNativeRef,
    declaration,
    measuredOccurrence,
    candidateRows,
    nativeReferenceDescriptors,
    nativeProofRows,
    groupsByDeclarationRef,
  };
}
