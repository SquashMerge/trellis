import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { lookupKey } from "../lookup.mjs";
import { parseJson } from "../json.mjs";
import {
  digest,
  syntaxId,
  occurrenceId,
  headerHash,
  siblingGroupHash,
  sourceManifestHash,
  identityRegistry,
  assignOrdinals,
  assignOccurrenceOrdinals,
  extractionContext,
} from "../identity.mjs";
import { validate } from "../formats.mjs";
import { canonicalBytes } from "../json.mjs";
const vectors = JSON.parse(
  readFileSync(
    new URL(
      "../../../docs/semantic-evidence/id-test-vectors/stable-ids.json",
      import.meta.url,
    ),
  ),
);
test("IDENTITY.VECTORS all 64 full digests, domains and canonical inputs from descriptors", () => {
  assert.equal(vectors.cases.length, 64);
  for (const item of vectors.cases) {
    const {
      sourceSet,
      path,
      language,
      ancestors,
      declaration,
      siblingHeaders,
    } = item.descriptor;
    const syntax = { sourceSet, path, language, ancestors, declaration };
    const headers = siblingHeaders.map(headerHash);
    for (const row of item.digests) {
      const [kind, input, domain] =
        row.label === "syntax"
          ? ["syntax", syntax, "trellis.syntax.v1\0"]
          : row.label === "sibling-group"
            ? ["siblingGroup", { headers }, "trellis.sibling-group.v1\0"]
            : [
                "header",
                siblingHeaders[Number(row.label.slice(-1))],
                "trellis.header.v1\0",
              ];
      assert.equal(
        Buffer.from(domain).toString("hex"),
        row.domainHex,
        item.caseId,
      );
      assert.equal(
        canonicalBytes(input).toString("hex"),
        row.inputHex,
        item.caseId,
      );
      assert.equal(digest(kind, input), row.sha256, item.caseId);
    }
    assert.equal(syntaxId(syntax), item.expected.stableId, item.caseId);
    assert.equal(item.expected.anchor.syntaxId, syntaxId(syntax), item.caseId);
    assert.deepEqual(
      item.expected.anchor.document,
      { sourceSetId: sourceSet, language, path },
      item.caseId,
    );
    assert.equal(
      item.expected.anchor.capturedRevisionId,
      item.descriptor.revisionId,
      item.caseId,
    );
    assert.equal(
      item.expected.anchor.siblingCount,
      siblingHeaders.length,
      item.caseId,
    );
    assert.equal(
      item.expected.anchor.identicalHeaderCount,
      headers.filter((hash) => hash === headerHash(item.descriptor.header))
        .length,
      item.caseId,
    );
    assert.equal(
      headerHash(item.descriptor.header),
      item.expected.anchor.headerHash,
      item.caseId,
    );
    assert.equal(
      siblingGroupHash(headers),
      item.expected.anchor.siblingGroupHash,
      item.caseId,
    );
  }
});
// Decision 0003 vectors, copied byte for byte from
// docs/semantic-evidence/publication-rejoin-vectors-v1.md#occurrence-identity-v2-decision-0003.
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const v2 = Object.freeze({
  contentHash:
    "8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2",
  owner: "sid:v1:7fd250597c82d08fcb73cabd62e89893",
  config1: "e3155b20e134632816c8611c4e9ee5cbd0e00689f7c4c955ee9f896580d02fdb",
  config2: "3e8214adf35212b25f8d669f6bb1416d39e07bacc3d82acfc287218cabbe0712",
  context1: "b49d85d64bb03f7cf62bd68c08f2a1aa091107769c325315528593d5724b12bc",
  context2: "1f2a121e20976ce130efa9cafff90f03229eab048982670f3a67fcf6b6b27ded",
});
const v2Input = (kind, version, context) => ({
  contentHash: v2.contentHash,
  extractionContext: context,
  nativeProducerId: "native-test",
  nativeProducerVersion: version,
  ownerSyntaxId: v2.owner,
  kind,
  ordinal: 0,
});
const v2Occurrences = [
  [
    v2Input("call", "native-test-1", v2.context1),
    `{"contentHash":"${v2.contentHash}","extractionContext":"${v2.context1}","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"${v2.owner}"}`,
    "38ec6920f23b3e1f7bb35550d019bbb622a70d87072c1bf6aba9c19974237cae",
    "occ:v2:38ec6920f23b3e1f7bb35550d019bbb6",
  ],
  [
    v2Input("reference", "native-test-1", v2.context1),
    `{"contentHash":"${v2.contentHash}","extractionContext":"${v2.context1}","kind":"reference","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"${v2.owner}"}`,
    "3445e26a698a121224048f0b37496cea58bc2d4b3a19a64d56851d57891041dc",
    "occ:v2:3445e26a698a121224048f0b37496cea",
  ],
  [
    v2Input("call", "native-test-2", v2.context1),
    `{"contentHash":"${v2.contentHash}","extractionContext":"${v2.context1}","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-2","ordinal":0,"ownerSyntaxId":"${v2.owner}"}`,
    "76c6e398193fe21ee1d9915dbbdee28db8d78c0e88d5c36d601c3b3787a4dd1f",
    "occ:v2:76c6e398193fe21ee1d9915dbbdee28d",
  ],
  [
    v2Input("call", "native-test-1", v2.context2),
    `{"contentHash":"${v2.contentHash}","extractionContext":"${v2.context2}","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"${v2.owner}"}`,
    "7850439bee41f2d40363662d283ffec3199ee742e3bbcf8c4d25e85968da6394",
    "occ:v2:7850439bee41f2d40363662d283ffec3",
  ],
];
test("IDENTITY.EXTRACTION_CONTEXT Decision 0003 vectors: capture digests, canonical bytes and SHA-256", () => {
  assert.equal(sha256("config-v1"), v2.config1);
  assert.equal(sha256("config-v2"), v2.config2);
  assert.equal(
    Buffer.from("trellis.extraction-context.v1\0").toString("hex"),
    "7472656c6c69732e65787472616374696f6e2d636f6e746578742e763100",
  );
  for (const [hash, literal, full] of [
    [
      v2.config1,
      `{"components":[{"hash":"${v2.config1}","name":"config"}],"language":"javascript"}`,
      v2.context1,
    ],
    [
      v2.config2,
      `{"components":[{"hash":"${v2.config2}","name":"config"}],"language":"javascript"}`,
      v2.context2,
    ],
  ]) {
    const input = {
      language: "javascript",
      components: [{ name: "config", hash }],
    };
    assert.equal(canonicalBytes(input).toString("utf8"), literal);
    assert.equal(extractionContext(input), full);
    assert.equal(digest("extractionContext", input), full);
  }
});
test("IDENTITY.OCCURRENCE Decision 0003 occ:v2 vectors byte for byte", () => {
  assert.equal(
    Buffer.from("trellis.occurrence.v2\0").toString("hex"),
    "7472656c6c69732e6f6363757272656e63652e763200",
  );
  for (const [input, literal, full, id] of v2Occurrences) {
    assert.equal(canonicalBytes(input).toString("utf8"), literal);
    assert.equal(digest("occurrence", input), full);
    assert.equal(occurrenceId(input), id);
    assert.equal(identityRegistry().register("occurrence", input), id);
  }
  // r1 ID = r2 ID: neither revision is an input. The two controls differ.
  const [call, reference, version, config] = v2Occurrences.map((x) => x[3]);
  assert.equal(new Set([call, reference, version, config]).size, 4);
});
test("IDENTITY.OCCURRENCE withdrawn revision-bound occ:v1 input and values are rejected", () => {
  // The withdrawn v1 r1/call row: domain trellis.occurrence.v1\0 over
  // {kind,ordinal,ownerSyntaxId,revisionId}, recomputed here independently.
  const v1Input = {
    revisionId: "r1",
    ownerSyntaxId: v2.owner,
    kind: "call",
    ordinal: 0,
  };
  const v1Full = sha256(
    Buffer.concat([
      Buffer.from("trellis.occurrence.v1\0"),
      canonicalBytes(v1Input),
    ]),
  );
  assert.ok(v1Full.startsWith("959c5606"));
  for (const bad of [
    v1Input,
    { ...v2Occurrences[0][0], revisionId: "r1" },
    (({ contentHash, ...rest }) => rest)(v2Occurrences[0][0]),
  ]) {
    assert.throws(() => digest("occurrence", bad), /FORMAT|field/);
    assert.throws(() => occurrenceId(bad), /FORMAT|field/);
    assert.throws(
      () => identityRegistry().register("occurrence", bad),
      /FORMAT|field/,
    );
  }
  const v1Id = `occ:v1:${v1Full.slice(0, 32)}`;
  for (const [, , full, id] of v2Occurrences) {
    assert.notEqual(full, v1Full);
    assert.notEqual(id, v1Id);
    assert.notEqual(id.slice(7), v1Id.slice(7));
  }
  assert.throws(() => validate("OccurrenceId", v1Id), /expected occurrenceId/);
  validate("OccurrenceId", v2Occurrences[0][3]);
});
test("IDENTITY.OCCURRENCE same document, producer and context share an ID; any input change re-identifies", () => {
  const base = v2Occurrences[0][0],
    id = occurrenceId(base);
  // Revisions r1 and r2 with identical bytes/producer/context: same ID.
  const registry = identityRegistry();
  assert.equal(registry.register("occurrence", structuredClone(base)), id);
  assert.equal(registry.register("occurrence", structuredClone(base)), id);
  for (const changed of [
    { ...base, contentHash: sha256("changed bytes") },
    { ...base, extractionContext: v2.context2 },
    { ...base, nativeProducerVersion: "native-test-2" },
    { ...base, nativeProducerId: "other-native" },
    { ...base, ordinal: 1 },
    { ...base, kind: "control" },
  ])
    assert.notEqual(occurrenceId(changed), id);
  const empty = extractionContext({ language: "javascript", components: [] });
  assert.equal(
    empty,
    sha256(
      Buffer.concat([
        Buffer.from("trellis.extraction-context.v1\0"),
        Buffer.from('{"components":[],"language":"javascript"}'),
      ]),
    ),
  );
  assert.notEqual(
    empty,
    extractionContext({ language: "python", components: [] }),
  );
  const a = { name: "config", hash: v2.config1 },
    b = { name: "dependency", hash: v2.config2 };
  extractionContext({ language: "javascript", components: [a, b] });
  for (const components of [
    [b, a],
    [a, a],
    [a, { name: "config", hash: v2.config2 }],
  ])
    assert.throws(
      () => extractionContext({ language: "javascript", components }),
      /IDENTITY.EXTRACTION_CONTEXT/,
    );
});
test("IDENTITY.COLLISION retained revisions and distinct full hashes sharing a prefix", () => {
  const input = {
    sourceSet: "core",
    path: "src/A.java",
    language: "java",
    ancestors: [],
    declaration: vectors.cases[0].descriptor.declaration,
  };
  const prefix = "a".repeat(32),
    registry = identityRegistry(
      (_, row) => prefix + (row.path === "src/B.java" ? "b" : "c").repeat(32),
    );
  assert.equal(
    registry.register("syntax", input),
    registry.register("syntax", structuredClone(input)),
  );
  assert.throws(
    () => registry.register("syntax", { ...input, path: "src/B.java" }),
    /IDENTITY.COLLISION/,
  );
  const item = {
    contentHash: "1".repeat(64),
    extractionContext: "2".repeat(64),
    nativeProducerId: "native",
    nativeProducerVersion: "1",
    ownerSyntaxId: syntaxId(input),
    kind: "call",
    ordinal: 0,
  };
  const occurrences = identityRegistry(
    (_, row) =>
      prefix + (row.contentHash === item.contentHash ? "c" : "b").repeat(32),
  );
  assert.equal(
    occurrences.register("occurrence", item),
    occurrences.register("occurrence", structuredClone(item)),
  );
  assert.throws(
    () =>
      occurrences.register("occurrence", {
        ...item,
        contentHash: "3".repeat(64),
      }),
    /IDENTITY.COLLISION/,
  );
  assert.throws(
    () => syntaxId({ ...input, nativeId: "ignored" }),
    /unknown field/,
  );
  assert.throws(
    () => syntaxId({ ...input, lookupKey: "normalized" }),
    /unknown field/,
  );
});
test("IDENTITY.ORDINAL immutable document, parent, exact name and signature scopes", () => {
  const common = {
    sourceSetId: "core",
    language: "rust",
    documentPath: "a.rs",
    revisionId: "r1",
    container: [],
    kind: "function",
    name: "x",
    signature: null,
  };
  const a = { ...common, range: { start: 8, end: 9 }, nativeId: "node-1" };
  const b = { ...common, range: { start: 2, end: 3 }, nativeId: "node-2" };
  const different = [
    { ...a, documentPath: "b.rs" },
    { ...a, revisionId: "r2" },
    { ...a, sourceSetId: "other" },
    {
      ...a,
      container: [
        { kind: "type", name: "Parent", signature: null, ordinal: 0 },
      ],
    },
    { ...a, name: null },
    { ...a, name: "é" },
    { ...a, name: "é" },
    {
      ...a,
      kind: "method",
      signature: {
        parameterTypes: ["int"],
        typeParameterCount: 0,
        variadic: false,
      },
    },
    {
      ...a,
      kind: "method",
      signature: {
        parameterTypes: ["long"],
        typeParameterCount: 0,
        variadic: false,
      },
    },
  ];
  const ordinals = assignOrdinals([a, b, ...different]);
  assert.equal(ordinals.get(b), 0);
  assert.equal(ordinals.get(a), 1);
  for (const row of different) assert.equal(ordinals.get(row), 0);
  assert.throws(
    () =>
      assignOrdinals([a, { ...a, lookupKey: "different", nativeId: "other" }]),
    /IDENTITY.ORDINAL/,
  );
  const javascript = {
    ...a,
    language: "javascript",
    nativeId: "other-language",
  };
  const documents = assignOrdinals([a, javascript]);
  assert.equal(documents.get(a), 0);
  assert.equal(documents.get(javascript), 0);
  const typescript = { ...a, language: "typescript", nativeId: "ts-language" };
  assert.equal(assignOrdinals([a, javascript, typescript]).get(typescript), 0);
  assert.throws(
    () => assignOrdinals([a, { ...a, nativeId: "same-language" }]),
    /duplicate native range/,
  );
  assert.throws(
    () => assignOrdinals([{ ...a, language: undefined }]),
    /FORMAT.SHAPE Language/,
  );
  assert.throws(
    () => assignOrdinals([{ ...a, language: "unknown" }]),
    /FORMAT.SHAPE Language/,
  );
  assert.throws(
    () =>
      assignOrdinals([
        {
          container: [],
          kind: "function",
          name: "x",
          signature: null,
          range: { start: 0, end: 1 },
        },
      ]),
    /invalid snapshot/,
  );
  const top = {
    sourceSet: "core",
    path: "a.rs",
    language: "rust",
    ancestors: [],
    declaration: { kind: "function", name: "x", signature: null, ordinal: 0 },
  };
  assert.equal(syntaxId(top), syntaxId({ ...top, ancestors: [] }));
  assert.notEqual(
    syntaxId(top),
    syntaxId({
      ...top,
      ancestors: [{ kind: "module", name: null, signature: null, ordinal: 0 }],
    }),
  );
  assert.notEqual(
    syntaxId({ ...top, declaration: { ...top.declaration, name: "é" } }),
    syntaxId({
      ...top,
      declaration: { ...top.declaration, name: lookupKey("rust", "é") },
    }),
  );
});
test("IDENTITY.MANIFEST pinned empty and populated bytes, sorted and sensitive", () => {
  const row = {
    document: { sourceSetId: "core", language: "java", path: "A.java" },
    contentHash: "a".repeat(64),
  };
  assert.equal(
    sourceManifestHash([]),
    "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945",
  );
  assert.equal(
    sourceManifestHash([row]),
    "7a71de35c8584c011a3bf5dcaf9940da12180667fb49d1261d4a1e9ca094c5b5",
  );
  assert.notEqual(
    sourceManifestHash([row]),
    sourceManifestHash([{ ...row, contentHash: "b".repeat(64) }]),
  );
  assert.throws(() => sourceManifestHash([row, row]), /IDENTITY.MANIFEST/);
  const rust = {
    document: { ...row.document, language: "rust", path: "z.rs" },
    contentHash: row.contentHash,
  };
  assert.throws(() => sourceManifestHash([rust, row]), /unsorted/);
  assert.throws(
    () =>
      sourceManifestHash([
        row,
        { ...row, document: { ...row.document, path: "0.java" } },
      ]),
    /unsorted/,
  );
  assert.match(sourceManifestHash([row, rust]), /^[a-f0-9]{64}$/);
  const javascript = {
    document: { ...row.document, language: "javascript", path: "z.js" },
    contentHash: row.contentHash,
  };
  const typescript = {
    document: { ...row.document, language: "typescript", path: "a.ts" },
    contentHash: row.contentHash,
  };
  assert.match(
    sourceManifestHash([row, rust, javascript, typescript]),
    /^[a-f0-9]{64}$/,
  );
  assert.throws(
    () => sourceManifestHash([typescript, javascript]),
    /IDENTITY.MANIFEST unsorted/,
  );
});
test("IDENTITY.CANONICAL every control and closed nested input", () => {
  const controls = Array.from(
    { length: 32 },
    (_, n) => `\\u00${n.toString(16).padStart(2, "0")}`,
  ).join("");
  assert.equal(
    canonicalBytes(
      Array.from({ length: 32 }, (_, n) => String.fromCharCode(n)).join(""),
    ).toString("utf8"),
    `"${controls}"`,
  );
  assert.equal(
    canonicalBytes({ z: '"\\', a: "é😀\u2028\u2029" }).toString("utf8"),
    '{"a":"é😀\u2028\u2029","z":"\\"\\\\"}',
  );
  assert.equal(canonicalBytes("é").toString("hex"), "22c3a922");
  assert.equal(
    canonicalBytes({ b: 0, a: null }).toString("hex"),
    Buffer.from('{"a":null,"b":0}').toString("hex"),
  );
  for (const bad of [-0, 1.5, -1, Number.MAX_SAFE_INTEGER + 1])
    assert.throws(() => canonicalBytes(bad), /JSON.CANONICAL/);
  for (const bad of ['{"a":1,"a":2}', "-0", "1.5", "-1"])
    assert.throws(() => parseJson(bad), /JSON.INTAKE/);
  const syntax = {
    sourceSet: "core",
    path: "a.rs",
    language: "rust",
    ancestors: [],
    declaration: { kind: "function", name: "a", signature: null, ordinal: 0 },
  };
  assert.throws(
    () =>
      syntaxId({ ...syntax, declaration: { ...syntax.declaration, extra: 1 } }),
    { assertion: "FORMAT.SHAPE", message: /declaration\.extra: unknown field/ },
  );
  assert.throws(
    () =>
      syntaxId({ ...syntax, ancestors: [{ ...syntax.declaration, extra: 1 }] }),
    {
      assertion: "FORMAT.SHAPE",
      message: /ancestors\[0\]\.extra: unknown field/,
    },
  );
  assert.equal(canonicalBytes([]).toString("hex"), "5b5d");
});
test("IDENTITY.OCCURRENCE_ORDINAL owner and kind isolate measured namespaces", () => {
  const make = (ownerSyntaxId, kind, start) => ({
    revisionId: "r1",
    ownerSyntaxId,
    kind,
    range: { start, end: start + 1 },
  });
  const a = make(vectors.cases[0].expected.stableId, "call", 5);
  const b = make(a.ownerSyntaxId, "call", 1),
    c = make(a.ownerSyntaxId, "reference", 5);
  const d = make(vectors.cases[2].expected.stableId, "call", 5);
  const ordinals = assignOccurrenceOrdinals([a, b, c, d]);
  assert.equal(ordinals.get(a), 1);
  assert.equal(ordinals.get(b), 0);
  assert.equal(ordinals.get(c), 0);
  assert.equal(ordinals.get(d), 0);
  assert.throws(
    () => assignOccurrenceOrdinals([a, { ...a }]),
    /duplicate native range/,
  );
  for (const bad of [
    { ...a, range: { start: 5, end: 5 } },
    { ...a, range: { start: -1, end: 1 } },
    { ...a, revisionId: "" },
    { ...a, ownerSyntaxId: "native-id" },
  ])
    assert.throws(() => assignOccurrenceOrdinals([bad]), /IDENTITY.OCCURRENCE/);
});
