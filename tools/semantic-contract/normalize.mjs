import { validate } from "./formats.mjs";
import { canonicalBytes } from "./json.mjs";
import {
  identityRegistry,
  assignOrdinals,
  assignOccurrenceOrdinals,
  headerHash,
  siblingGroupHash,
  nativeOccurrenceContext,
} from "./identity.mjs";
import {
  toByteRange,
  verifyWitness,
  verifyCallSpelling,
} from "./coordinates.mjs";
import { lookupKey, validateRoles, applicableRoles } from "./lookup.mjs";
import {
  expectedFreshness,
  expectedStaleTarget,
  checkCapturedBasis,
} from "./check-freshness.mjs";

const encode = (x) => canonicalBytes(x).toString("hex");
const compare = (a, b) => Buffer.compare(canonicalBytes(a), canonicalBytes(b));
const order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const tuple = (set, rev) => JSON.stringify([set, rev]);
const sourceKey = (row) =>
  JSON.stringify([row.document.sourceSetId, row.revisionId, row.document.path]);
function fail(id, field, message) {
  const e = new Error(`${id} ${field}: ${message}`);
  e.assertion = id;
  e.code = "invalidRecord";
  e.field = field;
  throw e;
}
function requireValue(value, id, field) {
  if (value === undefined) fail(id, field, "missing source-derived value");
  return value;
}
function unique(rows, key, id, field) {
  const seen = new Set();
  for (const row of rows) {
    const k = key(row);
    if (seen.has(k)) fail(id, field, "duplicate measured row");
    seen.add(k);
  }
}
function indexed(rows) {
  return new Map(rows.map((x) => [x.ref, x]));
}
function same(a, b) {
  return encode(a) === encode(b);
}
function encoding(range, expected, field) {
  if (range?.encoding !== expected)
    fail("NORMALIZE.ENCODING", field, "producer position encoding differs");
}
function producerEncoding(loaded, id) {
  return requireValue(
    loaded.fixture.producers.find((x) => x.id === id),
    "NORMALIZE.PRODUCER",
    "producerId",
  ).positionEncoding;
}
function contains(outer, inner) {
  return inner.start >= outer.start && inner.end <= outer.end;
}
function sourceLeaves(row) {
  const leaves = [];
  const add = (field, value, range = null) => {
    if (value !== null) leaves.push({ field, value, range });
  };
  if (row.header) {
    add("name", row.name, row.nameRange);
    add("header.name", row.header.name, row.nameRange);
    row.header.modifiers.forEach((x, i) => add(`header.modifiers[${i}]`, x));
    row.header.typeParameters.forEach((x, i) =>
      add(`header.typeParameters[${i}]`, x),
    );
    row.header.parameters.forEach((param, i) => {
      add(`header.parameters[${i}].name`, param.name);
      add(`header.parameters[${i}].type`, param.type);
    });
    add("header.resultType", row.header.resultType);
    row.header.bases.forEach((x, i) => add(`header.bases[${i}]`, x));
    if (row.signature)
      row.signature.parameterTypes.forEach((x, i) =>
        add(`signature.parameterTypes[${i}]`, x),
      );
  } else if (Object.hasOwn(row, "spelling"))
    add(
      "spelling",
      row.spelling,
      row.calleeRange ?? (row.regionRefs ? null : row.range),
    );
  return leaves;
}
function checkWitnesses(row, source, within, expectedEncoding) {
  const leaves = sourceLeaves(row),
    expected = new Map(leaves.map((x) => [x.field, x]));
  if (expected.size !== leaves.length)
    fail("NORMALIZE.WITNESS", "field", "duplicate source leaf");
  const seen = new Set();
  for (const entry of row.witnesses) {
    const leaf = expected.get(entry.field);
    if (!leaf || seen.has(entry.field))
      fail("NORMALIZE.WITNESS", entry.field, "unknown or duplicate witness");
    seen.add(entry.field);
    encoding(entry.witness.range, expectedEncoding, `witnesses.${entry.field}`);
    const actual = verifyWitness(source, entry.witness, { within });
    if (
      entry.witness.text !== leaf.value ||
      (leaf.range !== null &&
        (encoding(leaf.range, expectedEncoding, entry.field),
        !same(actual, toByteRange(source, leaf.range))))
    )
      fail("NORMALIZE.WITNESS", entry.field, "source leaf differs");
  }
  for (const leaf of leaves)
    if (!seen.has(leaf.field))
      fail("WITNESS.MISSING", leaf.field, "source leaf witness missing");
}
function symbolKey(key) {
  if ((key.scope === "document") !== (key.document !== null))
    fail("NORMALIZE.SYMBOL", "key", "symbol scope/document mismatch");
}

function sortRecords(rows) {
  return rows.sort((a, b) =>
    a.syntaxId && b.syntaxId
      ? order(a.syntaxId, b.syntaxId) ||
        order(a.revisionId, b.revisionId) ||
        compare(a.document, b.document)
      : a.id && b.id
        ? order(a.id, b.id) || compare(a, b)
        : compare(a, b),
  );
}
function insert(map, ref, value) {
  if (map.has(ref))
    fail("NORMALIZE.RECORD_REF", "recordRef", "conflicting record reference");
  map.set(ref, value);
}
function resolveTarget(ref, ids, decls, context = null) {
  if (ref === null) return null;
  if (ref.kind === "external") return { kind: "external", symbol: ref.symbol };
  const row = requireValue(
    decls.get(ref.declarationRef),
    "NORMALIZE.TARGET",
    "declarationRef",
  );
  if (row.revisionId !== ref.revisionId)
    fail("NORMALIZE.TARGET", "revisionId", "target revision differs");
  // v1: an internal target has the revision of the fact that names it.
  if (
    context?.revisionId !== undefined &&
    row.revisionId !== context.revisionId
  )
    fail(
      "NORMALIZE.TARGET",
      "revisionId",
      "internal target must share its fact's revision",
    );
  if (context && row.document.sourceSetId !== context.sourceSetId)
    fail(
      "NORMALIZE.TARGET",
      "sourceSetId",
      "other source set requires external target",
    );
  return {
    kind: "internal",
    syntaxId: requireValue(
      ids.get(ref.declarationRef),
      "NORMALIZE.TARGET",
      "declarationRef",
    ),
    document: row.document,
    revisionId: row.revisionId,
  };
}
function distinctTargets(targets) {
  if (new Set(targets.map(encode)).size !== targets.length)
    fail("NORMALIZE.TARGET", "candidates", "duplicate target");
  return [...targets].sort(compare);
}
function resolution(target, candidates, status) {
  if (
    status === "resolved" &&
    target?.kind === "internal" &&
    !candidates.length
  )
    return;
  if (
    status === "external" &&
    target?.kind === "external" &&
    !candidates.length
  )
    return;
  if (
    status === "ambiguous" &&
    target === null &&
    distinctTargets(candidates).length >= 2
  )
    return;
  if (status === "unresolved" && target === null && !candidates.length) return;
  fail("NORMALIZE.RESOLUTION", "resolution", "target cardinality differs");
}

// Distinct measured identities can share an exact owner/family/range anchor.
export function buildAnchorIndex(measurements) {
  const index = new Map();
  for (const x of measurements) {
    const k = encode([x.anchor, x.ownerRef]);
    if (!index.has(k)) index.set(k, []);
    index.get(k).push(x);
  }
  return index;
}
export function joinAnchor(
  selector,
  index,
  loaded,
  ids = new Map(),
  expectedEncoding = null,
) {
  validate("AnchorSelector", selector);
  if (expectedEncoding !== null)
    encoding(selector.range, expectedEncoding, "anchor.range");
  const source = requireValue(
    loaded.sources.get(
      JSON.stringify([
        selector.document.sourceSetId,
        selector.revisionId,
        selector.document.path,
      ]),
    ),
    "JOIN.DOCUMENT",
    "document",
  );
  const snapshot = loaded.revisions.get(
    tuple(selector.document.sourceSetId, selector.revisionId),
  );
  const document = snapshot?.documents.find(
    (x) => encode(x.key) === encode(selector.document),
  );
  if (!document || document.contentHash !== selector.contentHash)
    fail(
      "JOIN.DOCUMENT",
      "contentHash",
      `anchor source tuple differs: expected ${document?.contentHash ?? "a captured document"}, actual ${selector.contentHash}`,
    );
  const range = toByteRange(source, selector.range);
  const anchor = {
    document: selector.document,
    revisionId: selector.revisionId,
    contentHash: selector.contentHash,
    range,
    kind: selector.kind,
  };
  const intent = loaded.fixture.coverageIntents.find(
    (x) =>
      x.producerId === loaded.native.producerId &&
      x.revisionId === selector.revisionId &&
      same(x.document, selector.document),
  );
  const support = intent?.measurementSupport.find(
    (x) => x.kind === selector.kind,
  );
  const matches = index.get(encode([anchor, selector.ownerRef])) ?? [];
  const candidateIds = [
    ...new Set(
      matches.map((x) =>
        requireValue(ids.get(x.ref) ?? x.id, "JOIN.CANDIDATE", "candidateIds"),
      ),
    ),
  ].sort(order);
  if (support?.available === false && candidateIds.length)
    fail(
      "JOIN.SUPPORT",
      "measurementSupport",
      "unsupported measured family has candidates",
    );
  const status =
    support?.available === false
      ? "unsupported"
      : candidateIds.length === 0
        ? "unmatched"
        : candidateIds.length === 1
          ? "exact"
          : "ambiguous";
  return {
    anchor,
    status,
    candidateIds: status === "unsupported" ? [] : candidateIds,
    diagnostic:
      status === "exact"
        ? null
        : status === "unsupported"
          ? support.diagnostic
          : status,
  };
}

export function assignKeys(loaded, registry = identityRegistry()) {
  const native = loaded.native,
    nativeEncoding = producerEncoding(loaded, native.producerId),
    declarations = indexed(native.declarations),
    ids = new Map(),
    entries = new Map(),
    active = new Set();
  const measured = new Map();
  const position = (row) => {
    const source = requireValue(
      loaded.sources.get(sourceKey(row)),
      "NORMALIZE.SOURCE",
      "document",
    );
    encoding(row.range, nativeEncoding, "range");
    if (row.nameRange !== null)
      encoding(row.nameRange, nativeEncoding, "nameRange");
    const range = toByteRange(source, row.range),
      nameRange =
        row.nameRange === null ? null : toByteRange(source, row.nameRange);
    if (range.start === range.end && row.kind !== "module")
      fail("NORMALIZE.RANGE", "range", "empty non-module declaration");
    if (
      (row.name === null) !== (nameRange === null) ||
      (["module", "anonymousFunction"].includes(row.kind)
        ? row.name !== null
        : row.name === null && row.kind !== "implementation")
    )
      fail("NORMALIZE.NAME", "nameRange", "name/kind nullability differs");
    if (nameRange && !contains(range, nameRange))
      fail("NORMALIZE.NAME", "nameRange", "outside declaration");
    if (row.header.kind !== row.kind || row.header.name !== row.name)
      fail("NORMALIZE.HEADER", "header", "header differs from declaration");
    if (
      row.signature &&
      (row.signature.parameterTypes.length !== row.header.parameters.length ||
        row.signature.typeParameterCount !== row.header.typeParameters.length ||
        row.signature.variadic !==
          row.header.parameters.some((x) => x.variadic))
    )
      fail("NORMALIZE.SIGNATURE", "signature", "header disagrees");
    checkWitnesses(row, source, range, nativeEncoding);
    return { range, nameRange };
  };
  for (const row of native.declarations) measured.set(row.ref, position(row));
  const records = [],
    pending = new Set(native.declarations.map((x) => x.ref));
  while (pending.size) {
    const ready = native.declarations.filter(
      (x) =>
        pending.has(x.ref) &&
        (x.parentRef === null || !pending.has(x.parentRef)),
    );
    if (!ready.length)
      fail("NORMALIZE.OWNER", "parentRef", "declaration cycle");
    const ordinalRows = ready.map((row) => {
      let ancestors = [];
      if (row.parentRef === null && row.kind !== "module") {
        const child = measured.get(row.ref).range;
        for (const candidate of native.declarations) {
          if (
            candidate.ref === row.ref ||
            sourceKey(candidate) !== sourceKey(row)
          )
            continue;
          const outer = measured.get(candidate.ref).range;
          if (contains(outer, child) && !same(outer, child))
            fail(
              "NORMALIZE.OWNER",
              "parentRef",
              "nested declaration requires immediate owner",
            );
        }
      }
      if (row.parentRef !== null) {
        const parent = requireValue(
          declarations.get(row.parentRef),
          "NORMALIZE.OWNER",
          "parentRef",
        );
        if (sourceKey(parent) !== sourceKey(row))
          fail("NORMALIZE.OWNER", "parentRef", "cross-document parent");
        const p = requireValue(
          entries.get(parent.ref),
          "NORMALIZE.OWNER",
          "parentRef",
        );
        const childRange = measured.get(row.ref).range;
        if (!contains(p.range, childRange))
          fail("NORMALIZE.OWNER", "range", "child outside parent");
        for (const candidate of native.declarations) {
          if (
            candidate.ref === row.ref ||
            candidate.ref === parent.ref ||
            sourceKey(candidate) !== sourceKey(row)
          )
            continue;
          const middle = measured.get(candidate.ref).range;
          if (
            contains(p.range, middle) &&
            contains(middle, childRange) &&
            !same(middle, p.range) &&
            !same(middle, childRange)
          )
            fail("NORMALIZE.OWNER", "parentRef", "not immediate container");
        }
        ancestors =
          parent.kind === "module" ? p.ancestors : [...p.ancestors, p.key];
      }
      return {
        row,
        sourceSetId: row.document.sourceSetId,
        language: row.document.language,
        documentPath: row.document.path,
        revisionId: row.revisionId,
        container: ancestors,
        kind: row.kind,
        name: row.name,
        signature: row.signature,
        range: measured.get(row.ref).range,
      };
    });
    const ordinals = assignOrdinals(ordinalRows);
    for (const entry of ordinalRows) {
      const { row } = entry,
        key = {
          kind: row.kind,
          name: row.name,
          signature: row.signature,
          ordinal: ordinals.get(entry),
        },
        ancestors = entry.container;
      const syntaxId = registry.register("syntax", {
        sourceSet: row.document.sourceSetId,
        path: row.document.path,
        language: row.document.language,
        ancestors,
        declaration: key,
      });
      ids.set(row.ref, syntaxId);
      entries.set(row.ref, { key, ancestors, range: entry.range });
      pending.delete(row.ref);
      const out = {
        role: row.role,
        syntaxId,
        document: row.document,
        revisionId: row.revisionId,
        kind: row.kind,
        name: row.name,
        lookupKey:
          row.name === null ? null : lookupKey(row.document.language, row.name),
        ancestors,
        key,
        range: entry.range,
        nameRange: measured.get(row.ref).nameRange,
        header: row.header,
        provenanceId: `native:${row.revisionId}:${syntaxId}`,
      };
      validate("Declaration", out);
      records.push(out);
    }
  }
  const verified = new Map();
  for (const record of records) {
    const k = tuple(record.document.sourceSetId, record.revisionId);
    if (!verified.has(k)) verified.set(k, new Map());
    const entries = verified.get(k);
    if (
      entries.has(record.syntaxId) &&
      encode(entries.get(record.syntaxId)) !== encode(record.document)
    )
      fail("NORMALIZE.COLLISION", "syntaxId", "conflicting declaration");
    entries.set(record.syntaxId, record.document);
  }
  return { records: sortRecords(records), ids, verified, registry };
}

export function normalizeOccurrences(loaded, keys) {
  const { native } = loaded,
    nativeEncoding = producerEncoding(loaded, native.producerId),
    decl = indexed(native.declarations),
    controls = indexed(native.controls),
    ids = new Map(),
    measurements = [],
    rows = [];
  for (const [kind, items] of [
    ["call", native.calls],
    ["control", native.controls],
    ["reference", native.references],
  ])
    for (const row of items) {
      const owner = requireValue(
        decl.get(row.ownerRef),
        "NORMALIZE.OWNER",
        "ownerRef",
      );
      if (sourceKey(owner) !== sourceKey(row))
        fail("NORMALIZE.OWNER", "ownerRef", "cross-document occurrence");
      const source = requireValue(
        loaded.sources.get(sourceKey(row)),
        "NORMALIZE.SOURCE",
        "document",
      );
      encoding(row.range, nativeEncoding, "range");
      const range = toByteRange(source, row.range);
      const ownerRange = toByteRange(source, owner.range);
      if (range.start < ownerRange.start || range.end > ownerRange.end)
        fail("NORMALIZE.OWNER", "range", "outside owner");
      checkWitnesses(row, source, range, nativeEncoding);
      const own = requireValue(
        keys.ids.get(row.ownerRef),
        "NORMALIZE.OWNER",
        "ownerRef",
      );
      let calleeRange = null;
      if (kind === "call") {
        if (row.calleeRange !== null)
          encoding(row.calleeRange, nativeEncoding, "calleeRange");
        calleeRange =
          row.calleeRange === null
            ? null
            : toByteRange(source, row.calleeRange);
        if (
          calleeRange &&
          (calleeRange.start < range.start || calleeRange.end > range.end)
        )
          fail("NORMALIZE.CALLEE", "calleeRange", "outside invocation");
        if (!calleeRange && row.spelling !== null) {
          const witness = row.witnesses.find(
            (x) => x.field === "spelling",
          )?.witness;
          verifyCallSpelling(
            source,
            { calleeRange: null, spelling: row.spelling, range },
            witness,
          );
        }
      }
      if (
        kind === "reference" &&
        source.subarray(range.start, range.end).toString("utf8") !==
          row.spelling
      )
        fail("NORMALIZE.REFERENCE", "spelling", "reference bytes differ");
      rows.push({
        row,
        kind,
        range,
        calleeRange,
        revisionId: row.revisionId,
        ownerSyntaxId: own,
      });
    }
  const ordinals = assignOccurrenceOrdinals(rows);
  for (const item of rows) {
    item.ordinal = ordinals.get(item);
    const revision = loaded.revisions.get(
      tuple(item.row.document.sourceSetId, item.revisionId),
    );
    const document = revision.documents.find(
      (d) => encode(d.key) === encode(item.row.document),
    );
    item.id = keys.registry.register("occurrence", {
      ...nativeOccurrenceContext(loaded.fixture, native, revision, document),
      ownerSyntaxId: item.ownerSyntaxId,
      kind: item.kind,
      ordinal: item.ordinal,
    });
    ids.set(item.row.ref, item.id);
  }
  const all = rows.map((x) => {
    const { row, kind, range, id } = x,
      snapshot = loaded.revisions.get(
        tuple(row.document.sourceSetId, row.revisionId),
      ),
      document = snapshot.documents.find(
        (d) => encode(d.key) === encode(row.document),
      );
    const anchor = (family, r) => ({
      document: row.document,
      revisionId: row.revisionId,
      contentHash: document.contentHash,
      range: r,
      kind: family,
    });
    if (kind !== "control")
      measurements.push({
        ref: row.ref,
        id,
        ownerRef: row.ownerRef,
        anchor: anchor(kind === "call" ? "invocation" : "reference", range),
      });
    if (kind === "call" && x.calleeRange !== null)
      measurements.push({
        ref: row.ref,
        id,
        ownerRef: row.ownerRef,
        anchor: anchor("callee", x.calleeRange),
      });
    return x;
  });
  for (const row of native.declarations) {
    if (row.nameRange === null) continue;
    const snap = loaded.revisions.get(
      tuple(row.document.sourceSetId, row.revisionId),
    );
    const doc = snap.documents.find(
      (d) => encode(d.key) === encode(row.document),
    );
    measurements.push({
      ref: row.ref,
      id: keys.ids.get(row.ref),
      ownerRef: row.parentRef ?? row.ref,
      anchor: {
        document: row.document,
        revisionId: row.revisionId,
        contentHash: doc.contentHash,
        range: toByteRange(loaded.sources.get(sourceKey(row)), row.nameRange),
        kind: "declarationName",
      },
    });
  }
  const callRows = all.filter((x) => x.kind === "call"),
    controlRows = all.filter((x) => x.kind === "control"),
    referenceRows = all.filter((x) => x.kind === "reference");
  const byControl = indexed(native.controls),
    visiting = new Set(),
    complete = new Set();
  function controlChain(ref) {
    if (complete.has(ref)) return;
    if (visiting.has(ref)) fail("NORMALIZE.CONTROL", "parentRef", "cycle");
    visiting.add(ref);
    const row = requireValue(
      byControl.get(ref),
      "NORMALIZE.CONTROL",
      "parentRef",
    );
    if (row.parentRef !== null) {
      const p = requireValue(
        byControl.get(row.parentRef),
        "NORMALIZE.CONTROL",
        "parentRef",
      );
      if (p.ownerRef !== row.ownerRef || sourceKey(p) !== sourceKey(row))
        fail("NORMALIZE.CONTROL", "parentRef", "cross-owner parent");
      controlChain(p.ref);
      const inner = controlRows.find((x) => x.row === row).range,
        outer = controlRows.find((x) => x.row === p).range;
      if (inner.start < outer.start || inner.end > outer.end)
        fail("NORMALIZE.CONTROL", "range", "outside parent");
    }
    visiting.delete(ref);
    complete.add(ref);
  }
  for (const row of native.controls) controlChain(row.ref);
  const regions = controlRows.map((x) => ({
    id: x.id,
    ownerSyntaxId: x.ownerSyntaxId,
    ordinal: x.ordinal,
    document: x.row.document,
    revisionId: x.revisionId,
    kind: x.row.kind,
    range: x.range,
    parentId: x.row.parentRef === null ? null : ids.get(x.row.parentRef),
    arm: x.row.arm,
    provenanceId: `native:${x.revisionId}:${x.id}`,
  }));
  const calls = callRows.map((x) => {
    const seen = new Set();
    let last = null;
    for (const ref of x.row.regionRefs) {
      if (seen.has(ref))
        fail("NORMALIZE.REGION", "regionRefs", "duplicate region");
      seen.add(ref);
      const region = requireValue(
        byControl.get(ref),
        "NORMALIZE.REGION",
        "regionRefs",
      );
      const area = controlRows.find((y) => y.row === region).range;
      if (
        region.ownerRef !== x.row.ownerRef ||
        !contains(area, x.range) ||
        (last !== null && region.parentRef !== last)
      )
        fail(
          "NORMALIZE.REGION",
          "regionRefs",
          "call outside ordered control chain",
        );
      last = ref;
    }
    return {
      id: x.id,
      ownerSyntaxId: x.ownerSyntaxId,
      ordinal: x.ordinal,
      document: x.row.document,
      revisionId: x.revisionId,
      range: x.range,
      calleeRange: x.calleeRange,
      spelling: x.row.spelling,
      regionIds: x.row.regionRefs.map((ref) => ids.get(ref)),
      provenanceId: `native:${x.revisionId}:${x.id}`,
    };
  });
  for (const x of [...regions, ...calls])
    validate(x.regionIds ? "Call" : "ControlRegion", x);
  return {
    calls: sortRecords(calls),
    controlRegions: sortRecords(regions),
    referenceRows,
    ids,
    index: buildAnchorIndex(measurements),
  };
}

export function collapseFacts(rows) {
  const by = new Map();
  for (const row of rows) {
    const key = encode(row);
    if (!by.has(key)) by.set(key, row);
  }
  return sortRecords([...by.values()]);
}

function nativeProof(row, loaded, id) {
  const snapshot = loaded.revisions.get(
      tuple(row.document.sourceSetId, row.revisionId),
    ),
    document = snapshot.documents.find(
      (x) => encode(x.key) === encode(row.document),
    );
  const proof = {
    id,
    producerId: loaded.native.producerId,
    document: row.document,
    revisionId: row.revisionId,
    contentHash: document.contentHash,
    evidenceKind: "measuredSyntax",
    basis: null,
    freshness: "fresh",
  };
  proof.freshness = expectedFreshness(proof, loaded);
  validate("Provenance", proof);
  return proof;
}
function normalizeRelationship(fact, ids, decls, context = null) {
  const record = {
    kind: fact.relationshipKind,
    source: resolveTarget(fact.source, ids, decls, context),
    target: resolveTarget(fact.target, ids, decls, context),
    provenanceId: fact.provenanceRef,
  };
  if (record.target.kind === "external") symbolKey(record.target.symbol);
  if (record.source.kind !== "internal")
    fail("NORMALIZE.RELATIONSHIP", "source", "internal source required");
  validate("TypeRelationship", record);
  return record;
}
export { normalizeRelationship };

export function normalizeFixture(loaded) {
  if (
    !loaded?.native ||
    !loaded?.fixture ||
    !loaded?.sources ||
    !loaded?.selected
  )
    fail("NORMALIZE.INPUT", "loaded", "admitted fixture required");
  validate("FixtureV1", loaded.fixture);
  validate("NativeArtifact", loaded.native);
  unique(
    [
      ...loaded.native.declarations,
      ...loaded.native.calls,
      ...loaded.native.controls,
      ...loaded.native.references,
    ],
    (x) => x.ref,
    "NORMALIZE.NATIVE",
    "ref",
  );
  const keys = assignKeys(loaded),
    occ = normalizeOccurrences(loaded, keys),
    decl = indexed(loaded.native.declarations),
    nativeRows = [
      ...loaded.native.declarations,
      ...loaded.native.calls,
      ...loaded.native.controls,
      ...loaded.native.references,
    ];
  const records = {
    formatVersion: 1,
    comparison: structuredClone(loaded.comparison),
    producers: sortRecords(structuredClone(loaded.fixture.producers)),
    sourceSets: sortRecords(structuredClone(loaded.fixture.sourceSets)),
    revisions: sortRecords(
      [...loaded.revisions.values()].map((x) => structuredClone(x)),
    ),
    coverage: [],
    provenance: [],
    declarations: keys.records,
    symbols: [],
    declarationBindings: [],
    typeRelationships: [],
    calls: occ.calls,
    controlRegions: occ.controlRegions,
    references: [],
    referenceJoinDiagnostics: [],
    callBindings: [],
    durableAnchors: [],
    groupContinuities: [],
    anchorResults: [],
  };
  const recordMap = new Map(),
    identityMap = new Map([...keys.ids, ...occ.ids]);
  for (const row of nativeRows) {
    const output =
      records.declarations.find(
        (x) =>
          x.syntaxId === keys.ids.get(row.ref) &&
          x.revisionId === row.revisionId &&
          same(x.document, row.document),
      ) ??
      records.calls.find(
        (x) =>
          x.id === occ.ids.get(row.ref) && x.revisionId === row.revisionId,
      ) ??
      records.controlRegions.find(
        (x) =>
          x.id === occ.ids.get(row.ref) && x.revisionId === row.revisionId,
      );
    if (output) insert(recordMap, row.ref, output);
    const proof = nativeProof(
      row,
      loaded,
      `native:${row.revisionId}:${identityMap.get(row.ref)}`,
    );
    records.provenance.push(proof);
  }
  const factRefs = new Set(),
    provenances = new Map(records.provenance.map((x) => [x.id, x]));
  const facts = loaded.annotations.flatMap((x) => x.facts);
  for (const fact of facts) {
    validate("Fact", fact);
    if (
      identityMap.has(fact.ref) ||
      loaded.anchors.cases.some((x) => x.id === fact.ref)
    )
      fail(
        "NORMALIZE.RECORD_REF",
        "recordRef",
        "cross-namespace reference collision",
      );
    if (factRefs.has(fact.ref))
      fail(
        "NORMALIZE.FACT_REF",
        "ref",
        "duplicate fixture-wide fact reference",
      );
    factRefs.add(fact.ref);
    if (fact.kind !== "provenance") continue;
    const proof = {
      ...fact.record,
      freshness: expectedFreshness(fact.record, loaded),
    };
    checkCapturedBasis(proof, loaded);
    const old = provenances.get(proof.id);
    if (old && encode(old) !== encode(proof))
      fail("NORMALIZE.PROVENANCE", "id", "conflicting proof");
    provenances.set(proof.id, proof);
    records.provenance.push(proof);
    insert(recordMap, fact.ref, proof);
  }
  const getProof = (id) =>
    requireValue(provenances.get(id), "NORMALIZE.PROVENANCE", "provenanceId");
  const target = (ref, context) => {
    const value = resolveTarget(ref, keys.ids, decl, context);
    if (value?.kind === "external") symbolKey(value.symbol);
    return value;
  };
  const bindingGroups = new Map(),
    referenceGroups = new Map();
  for (const fact of facts) {
    if (fact.kind === "provenance") continue;
    if (fact.kind === "coverage") {
      validate("Coverage", fact.record);
      const value = fact.record;
      const annotation = loaded.annotations.find((x) => x.facts.includes(fact));
      if (
        !same(annotation.document, {
          sourceSetId: value.sourceSetId,
          language: value.language,
          path: value.documentPath,
        }) ||
        annotation.revisionId !== value.revisionId
      )
        fail("NORMALIZE.COVERAGE", "record", "annotation tuple mismatch");
      if (
        !loaded.fixture.producers.some(
          (x) =>
            x.id === value.producerId && x.languages.includes(value.language),
        ) ||
        !loaded.revisions
          .get(tuple(value.sourceSetId, value.revisionId))
          ?.documents.some(
            (x) =>
              x.key.path === value.documentPath &&
              x.key.language === value.language,
          )
      )
        fail("NORMALIZE.COVERAGE", "record", "unadmitted coverage tuple");
      const applicable = applicableRoles(value.language),
        ordered = (roles) =>
          roles.every(
            (role, i) =>
              applicable.includes(role) &&
              (i === 0 ||
                applicable.indexOf(role) > applicable.indexOf(roles[i - 1])),
          );
      const selected = ["failed", "partial", "complete"].includes(value.state),
        requested = value.state !== "notRequested";
      const intent = loaded.fixture.coverageIntents.find(
        (x) =>
          x.producerId === value.producerId &&
          x.revisionId === value.revisionId &&
          same(x.document, annotation.document),
      );
      const roles =
        intent?.requestedRoles.filter((x) => applicable.includes(x)) ?? [];
      const families = (role) =>
        role === "definition" || role === "alias"
          ? ["reference", "declarationName"]
          : ["reference"];
      const supports = (kind) =>
        intent?.measurementSupport.find((x) => x.kind === kind)?.available ===
        true;
      const available = (role) =>
        role === "call"
          ? supports("callee") || supports("invocation")
          : families(role).every(supports);
      const unsupported = roles.some(
        (x) => !value.supportedRoles.includes(x) || !available(x),
      );
      const missing = roles.some(
        (x) =>
          value.supportedRoles.includes(x) &&
          available(x) &&
          !value.observedRoles.includes(x),
      );
      const effective = roles.filter(
        (x) => value.supportedRoles.includes(x) && available(x),
      );
      if (
        value.requested !== requested ||
        value.selected !== selected ||
        (value.diagnostic === null) !==
          ["notRequested", "complete"].includes(value.state) ||
        !ordered(value.supportedRoles) ||
        !ordered(value.observedRoles) ||
        value.observedRoles.some((x) => !value.supportedRoles.includes(x)) ||
        value.observedRoles.some((x) => !roles.includes(x) || !available(x)) ||
        (requested && (!intent || !roles.length)) ||
        (!requested && roles.length) ||
        (value.state === "unsupported" && effective.length > 0) ||
        (value.state === "omitted" && !effective.length) ||
        (value.state === "complete" && (unsupported || missing)) ||
        (value.state === "partial" && !(unsupported || missing))
      )
        fail("NORMALIZE.COVERAGE", "record", "state/roles/diagnostic mismatch");
      records.coverage.push(value);
      insert(recordMap, fact.ref, value);
      continue;
    }
    const proof = getProof(
      fact.kind === "typeRelationship"
        ? fact.provenanceRef
        : fact.record.provenanceId,
    );
    const expectedKind = {
      symbol: "declarationBinding",
      declarationBinding: "declarationBinding",
      callBinding: "semanticReference",
      reference: "semanticReference",
      typeRelationship: "typeRelationship",
    }[fact.kind];
    if (proof.evidenceKind !== expectedKind)
      fail("NORMALIZE.EVIDENCE_KIND", "provenanceId", "wrong fact-class proof");
    const annotation = loaded.annotations.find((x) => x.facts.includes(fact));
    if (
      !same(proof.document, annotation.document) ||
      proof.revisionId !== annotation.revisionId
    )
      fail(
        "NORMALIZE.FACT_TUPLE",
        "provenanceId",
        "proof outside authored document/revision",
      );
    if (
      fact.anchor &&
      (!same(fact.anchor.document, proof.document) ||
        fact.anchor.revisionId !== proof.revisionId ||
        fact.anchor.contentHash !== proof.contentHash)
    )
      fail(
        "NORMALIZE.FACT_TUPLE",
        "anchor",
        `anchor/proof tuple mismatch${proof.contentHash !== fact.anchor.contentHash ? `: expected contentHash ${proof.contentHash}, actual ${fact.anchor.contentHash}` : ""}`,
      );
    if (fact.kind === "symbol") {
      symbolKey(fact.record.key);
      const value = {
        ...fact.record,
        declarations: fact.record.declarations.map((x) =>
          target(x, { ...proof.document, revisionId: proof.revisionId }),
        ),
      };
      if (
        new Set(value.declarations.map(encode)).size !==
        value.declarations.length
      )
        fail("NORMALIZE.SYMBOL", "declarations", "duplicate target");
      validate("Symbol", value);
      records.symbols.push(value);
      insert(recordMap, fact.ref, value);
      continue;
    }
    if (fact.kind === "typeRelationship") {
      if (proof.evidenceKind !== "typeRelationship")
        fail(
          "NORMALIZE.RELATIONSHIP",
          "provenanceRef",
          "wrong independent proof",
        );
      const value = normalizeRelationship(fact, keys.ids, decl, {
        ...proof.document,
        revisionId: proof.revisionId,
      });
      records.typeRelationships.push(value);
      insert(recordMap, fact.ref, value);
      continue;
    }
    if (
      (fact.kind === "declarationBinding" &&
        fact.anchor.kind !== "declarationName") ||
      (fact.kind === "reference" && fact.anchor.kind !== "reference") ||
      (fact.kind === "callBinding" &&
        !["callee", "invocation"].includes(fact.anchor.kind))
    )
      fail(
        "NORMALIZE.JOIN_FAMILY",
        "anchor.kind",
        "fact anchor family mismatch",
      );
    const join = joinAnchor(
      fact.anchor,
      occ.index,
      loaded,
      identityMap,
      producerEncoding(loaded, proof.producerId),
    );
    if (fact.kind === "declarationBinding") {
      const value = {
        syntaxId: join.status === "exact" ? join.candidateIds[0] : null,
        symbols: fact.record.symbols,
        join,
        provenanceId: proof.id,
      };
      if (
        (join.status === "exact" && !value.symbols.length) ||
        new Set(value.symbols.map(encode)).size !== value.symbols.length
      )
        fail(
          "NORMALIZE.DECLARATION_BINDING",
          "symbols",
          "missing/duplicate symbol",
        );
      for (const symbol of value.symbols) symbolKey(symbol);
      validate("DeclarationBinding", value);
      records.declarationBindings.push(value);
      insert(recordMap, fact.ref, value);
      continue;
    }
    if (fact.kind === "reference") {
      if (proof.evidenceKind !== "semanticReference")
        fail(
          "NORMALIZE.REFERENCE",
          "provenanceId",
          "semantic reference proof required",
        );
      if (join.status !== "exact") {
        const diagnostic = { factRef: fact.ref, provenanceId: proof.id, join };
        validate("ReferenceJoinDiagnostic", diagnostic);
        records.referenceJoinDiagnostics.push(diagnostic);
        insert(recordMap, fact.ref, diagnostic);
        continue;
      }
      const native = occ.referenceRows.find(
        (x) =>
          x.id === join.candidateIds[0] &&
          x.revisionId === join.anchor.revisionId,
      );
      if (!native)
        fail("NORMALIZE.REFERENCE", "anchor", "reference measurement missing");
      const row = native.row;
      const value = {
        id: native.id,
        ownerSyntaxId: native.ownerSyntaxId,
        ordinal: native.ordinal,
        document: row.document,
        revisionId: row.revisionId,
        range: native.range,
        spelling: row.spelling,
        lookupKey: lookupKey(row.document.language, row.spelling),
        site: fact.record.site,
        roles: fact.record.roles,
        resolution: fact.record.resolution,
        declaredTarget: target(fact.record.declaredTarget, {
          ...proof.document,
          revisionId: proof.revisionId,
        }),
        candidates: distinctTargets(
          fact.record.candidates.map((x) =>
            target(x, { ...proof.document, revisionId: proof.revisionId }),
          ),
        ),
        provenanceId: proof.id,
      };
      validateRoles(row.document.language, value.roles, {
        site: value.site,
        callee: occ.calls.some(
          (x) =>
            x.ownerSyntaxId === value.ownerSyntaxId &&
            x.calleeRange &&
            encode(x.calleeRange) === encode(value.range),
        ),
      });
      resolution(value.declaredTarget, value.candidates, value.resolution);
      validate("Reference", value);
      const referenceKey = encode([
        proof.producerId,
        value.revisionId,
        value.id,
      ]);
      if (!referenceGroups.has(referenceKey))
        referenceGroups.set(referenceKey, []);
      referenceGroups.get(referenceKey).push({ fact, value });
      insert(recordMap, fact.ref, value);
      continue;
    }
    if (fact.kind === "callBinding") {
      const value = {
        callId: join.status === "exact" ? join.candidateIds[0] : null,
        join,
        resolution: fact.record.resolution,
        declaredTarget: target(fact.record.declaredTarget, {
          ...proof.document,
          revisionId: proof.revisionId,
        }),
        candidates: distinctTargets(
          fact.record.candidates.map((x) =>
            target(x, { ...proof.document, revisionId: proof.revisionId }),
          ),
        ),
        dispatch: fact.record.dispatch,
        possibleDispatch: distinctTargets(
          fact.record.possibleDispatch.map((x) =>
            target(x, { ...proof.document, revisionId: proof.revisionId }),
          ),
        ),
        possibleDispatchComplete: false,
        staleTarget: null,
        provenanceId: proof.id,
      };
      resolution(value.declaredTarget, value.candidates, value.resolution);
      value.staleTarget = expectedStaleTarget(
        value,
        proof,
        loaded,
        keys.verified,
      );
      validate("CallBinding", value);
      const k = encode([
        proof.producerId,
        join.status === "exact" ? join.candidateIds[0] : join.anchor,
        join.anchor.document,
        join.anchor.revisionId,
      ]);
      if (!bindingGroups.has(k)) bindingGroups.set(k, []);
      bindingGroups.get(k).push({ fact, value, proof });
      insert(recordMap, fact.ref, value);
    }
  }
  for (const group of referenceGroups.values()) {
    const values = collapseFacts(group.map((x) => x.value)),
      first = values[0];
    const strip = ({
      resolution,
      declaredTarget,
      candidates,
      provenanceId,
      ...rest
    }) => rest;
    if (values.some((x) => !same(strip(x), strip(first))))
      fail(
        "NORMALIZE.REFERENCE_CONFLICT",
        "resolution",
        "contributor non-target claims differ",
      );
    const targets = [
      ...new Map(
        values.map((x) => [encode(x.declaredTarget), x.declaredTarget]),
      ).values(),
    ];
    const contradiction = targets.length > 1;
    if (
      (contradiction &&
        values.some(
          (x) =>
            !["resolved", "external"].includes(x.resolution) ||
            x.declaredTarget === null ||
            x.candidates.length,
        )) ||
      (!contradiction &&
        values.some(
          (x) =>
            !same(
              { ...x, provenanceId: null },
              { ...first, provenanceId: null },
            ),
        ))
    )
      fail(
        "NORMALIZE.REFERENCE_CONFLICT",
        "resolution",
        "incompatible duplicate semantic claims",
      );
    for (const x of values) {
      const value = contradiction
        ? {
            ...x,
            resolution: "ambiguous",
            declaredTarget: null,
            candidates: targets.sort(compare),
          }
        : x;
      validate("Reference", value);
      records.references.push(value);
      for (const entry of group)
        if (entry.value.provenanceId === x.provenanceId)
          recordMap.set(entry.fact.ref, value);
    }
  }
  for (const group of bindingGroups.values()) {
    const items = collapseFacts(group.map((x) => x.value));
    if (items.length === 1) {
      records.callBindings.push(items[0]);
      continue;
    }
    const baseline = items[0],
      same = items.every(
        (x) =>
          x.callId === baseline.callId &&
          encode(x.join) === encode(baseline.join) &&
          x.dispatch === baseline.dispatch &&
          encode(x.possibleDispatch) === encode(baseline.possibleDispatch),
      );
    if (!same)
      fail(
        "NORMALIZE.BINDING_CONFLICT",
        "declaredTarget",
        "contributor non-target claims differ",
      );
    const targets = distinctTargets([
      ...new Map(
        items.map((x) => [encode(x.declaredTarget), x.declaredTarget]),
      ).values(),
    ]);
    if (
      targets.length > 1 &&
      items.some(
        (x) =>
          x.join.status !== "exact" ||
          !["resolved", "external"].includes(x.resolution) ||
          x.declaredTarget === null ||
          x.candidates.length,
      )
    )
      fail(
        "NORMALIZE.BINDING_CONFLICT",
        "declaredTarget",
        "contradictory targets need exact resolved or external claims",
      );
    if (targets.length < 2) {
      if (
        items.some(
          (x) =>
            encode({ ...x, provenanceId: null }) !==
            encode({ ...baseline, provenanceId: null }),
        )
      )
        fail(
          "NORMALIZE.BINDING_CONFLICT",
          "declaredTarget",
          "conflicting duplicate facts",
        );
      records.callBindings.push(...items);
      continue;
    }
    for (const x of items) {
      const value = {
        ...x,
        resolution: "ambiguous",
        declaredTarget: null,
        candidates: targets,
        staleTarget: null,
      };
      validate("CallBinding", value);
      records.callBindings.push(value);
      for (const entry of group)
        if (entry.value.provenanceId === x.provenanceId)
          recordMap.set(entry.fact.ref, value);
    }
  }
  for (const anchorCase of loaded.anchors.cases) {
    if (identityMap.has(anchorCase.id))
      fail(
        "NORMALIZE.RECORD_REF",
        "recordRef",
        "anchor/native reference collision",
      );
    if (
      anchorCase.currentRevisionId !== loaded.comparison.revisionId ||
      anchorCase.continuity.fromRevisionId !==
        decl.get(anchorCase.capturedDeclarationRef)?.revisionId ||
      anchorCase.continuity.toRevisionId !== anchorCase.currentRevisionId
    )
      fail("NORMALIZE.ANCHOR", "continuity", "case/revision link differs");
    const captured = requireValue(
      decl.get(anchorCase.capturedDeclarationRef),
      "NORMALIZE.ANCHOR",
      "capturedDeclarationRef",
    );
    const declaration = requireValue(
      keys.records.find(
        (x) =>
          x.syntaxId === keys.ids.get(captured.ref) &&
          x.revisionId === captured.revisionId,
      ),
      "NORMALIZE.ANCHOR",
      "capturedDeclarationRef",
    );
    const siblings = keys.records
      .filter(
        (x) =>
          encode(x.document) === encode(declaration.document) &&
          x.revisionId === declaration.revisionId &&
          encode(x.ancestors) === encode(declaration.ancestors) &&
          x.key.kind === declaration.key.kind &&
          x.key.name === declaration.key.name &&
          encode(x.key.signature) === encode(declaration.key.signature),
      )
      .sort(
        (a, b) => a.range.start - b.range.start || a.range.end - b.range.end,
      );
    const focusedHash = headerHash(declaration.header);
    const headers = siblings.map((x) => headerHash(x.header));
    const durable = {
      syntaxId: declaration.syntaxId,
      document: declaration.document,
      capturedRevisionId: declaration.revisionId,
      headerHash: focusedHash,
      siblingGroupHash: siblingGroupHash(headers),
      siblingCount: siblings.length,
      identicalHeaderCount: headers.filter((x) => x === focusedHash).length,
    };
    validate("DurableAnchor", durable);
    records.durableAnchors.push(durable);
    records.groupContinuities.push(anchorCase.continuity);
    const answer = anchorCase.expectedResult;
    const targetId =
      answer.targetId === null
        ? null
        : typeof answer.targetId === "string"
          ? answer.targetId
          : requireValue(
              identityMap.get(answer.targetId.ref),
              "NORMALIZE.ANCHOR",
              "targetId",
            );
    if (
      anchorCase.continuity.fromRevisionId ===
        anchorCase.continuity.toRevisionId &&
      targetId !== declaration.syntaxId
    )
      fail("NORMALIZE.ANCHOR", "continuity", "same-revision identity differs");
    if (
      targetId !== null &&
      !keys.records.some(
        (x) =>
          x.syntaxId === targetId &&
          x.revisionId === anchorCase.currentRevisionId,
      )
    )
      fail("NORMALIZE.ANCHOR", "targetId", "target outside current revision");
    if (
      (answer.status === "attached") !== (targetId !== null) ||
      (answer.reason === "none") !== (answer.status === "attached")
    )
      fail(
        "NORMALIZE.ANCHOR",
        "expectedResult",
        "status/target/reason mismatch",
      );
    const result = { ...answer, targetId };
    validate("AnchorResult", result);
    records.anchorResults.push(result);
    insert(recordMap, anchorCase.id, durable);
  }
  for (const field of [
    "coverage",
    "provenance",
    "symbols",
    "declarationBindings",
    "typeRelationships",
    "references",
    "referenceJoinDiagnostics",
    "callBindings",
  ])
    records[field] = collapseFacts(records[field]);
  for (const [field, key] of [
    [
      "coverage",
      (x) =>
        encode([x.producerId, x.sourceSetId, x.documentPath, x.revisionId]),
    ],
    [
      "symbols",
      (x) => encode([provenances.get(x.provenanceId)?.producerId, x.key]),
    ],
    ["declarations", (x) => encode([x.revisionId, x.syntaxId])],
    ["calls", (x) => encode([x.revisionId, x.id])],
  ]) {
    const seen = new Map();
    for (const row of records[field]) {
      const identity = key(row),
        old = seen.get(identity);
      if (old && !same(old, row))
        fail("NORMALIZE.RECORD_REF", field, "conflicting normalized identity");
      seen.set(identity, row);
    }
  }
  for (const field of ["durableAnchors", "groupContinuities", "anchorResults"])
    records[field].sort(compare);
  records.referenceJoinDiagnostics.sort((a, b) => order(a.factRef, b.factRef));
  validate("NormalizedRecordsV1", records);
  return { records, identityMap, recordMap, declarations: keys.verified };
}
export const normalize = normalizeFixture;
