import test from "node:test";
import assert from "node:assert/strict";
import { schemas } from "../schema.mjs";
import { validate } from "../formats.mjs";
import { canonicalBytes, parseJson } from "../json.mjs";
import {
  controls,
  registerControls,
  runControl,
  schemaFieldControls,
} from "./mutations.mjs";

function sample(spec, trail = []) {
  if (typeof spec === "string" && Object.hasOwn(schemas, spec)) {
    if (trail.includes(spec)) throw new Error(`recursive schema: ${spec}`);
    return sample(schemas[spec], [...trail, spec]);
  }
  if (typeof spec === "string")
    return {
      text: "sample",
      uint: 1,
      boolean: false,
      hash: "a".repeat(64),
      path: "src/a.js",
      syntaxId: "sid:v1:" + "a".repeat(32),
      occurrenceId: "occ:v2:" + "a".repeat(32),
    }[spec];
  if (spec.nullable) return null;
  if (spec.either) return sample(spec.either[0], trail);
  if (Object.hasOwn(spec, "literal")) return spec.literal;
  if (spec.enum) return spec.enum[0];
  if (spec.array) return [];
  if (spec.union) return sample(Object.values(spec.union.variants)[0], trail);
  if (spec.object)
    return Object.fromEntries(
      Object.entries(spec.object).map(([k, v]) => [k, sample(v, trail)]),
    );
  throw Error(`unknown schema ${String(spec)}`);
}
function failure(type, value, field) {
  assert.throws(
    () => validate(type, value),
    (e) => e.assertion === "FORMAT.SHAPE" && e.field === field,
  );
}
test("FORMAT.REGISTRY: every type and every required object field is closed", async (t) => {
  for (const [name, spec] of Object.entries(schemas)) {
    await t.test(name, async () => {
      const value = sample(name);
      validate(name, value);
      if (spec.object) {
        for (const control of schemaFieldControls(name, value))
          await runControl(control);
        failure(name, { ...value, unexpected: true }, `${name}.unexpected`);
        for (const [field, fieldSpec] of Object.entries(spec.object)) {
          const bad = { ...value, [field]: null };
          if (
            !fieldSpec?.nullable &&
            !(fieldSpec?.literal && fieldSpec.literal === null)
          )
            failure(name, bad, `${name}.${field}`);
          if (fieldSpec?.nullable)
            failure(name, { ...value, [field]: undefined }, `${name}.${field}`);
        }
      }
    });
  }
});

test("FORMAT.UNIONS: every variant has closed fields", () => {
  for (const [name, spec] of Object.entries(schemas)) {
    if (!spec.union) continue;
    for (const [tag, variant] of Object.entries(spec.union.variants)) {
      const value = sample(variant);
      validate(name, value);
      failure(
        name,
        { ...value, legacyRelationship: "extends" },
        `${name}.legacyRelationship`,
      );
      const unknown = { ...value, [spec.union.tag]: "__unknown__" };
      failure(name, unknown, `${name}.${spec.union.tag}`);
    }
  }
});
test("RELATIONSHIP_FORMAT: explicit and independent relationshipKind for all three values", () => {
  const relationship = sample("TypeRelationshipFact");
  for (const relationshipKind of ["extends", "implements", "overrides"])
    validate("TypeRelationshipFact", { ...relationship, relationshipKind });
  for (const [value, field] of [
    [{ ...relationship, relationshipKind: null }, "relationshipKind"],
    [
      { ...relationship, relationshipKind: "typeRelationship" },
      "relationshipKind",
    ],
    [{ ...relationship, kind: "extends" }, "kind"],
    [
      {
        ...relationship,
        source: { kind: "external", symbol: sample("SymbolKey") },
      },
      "source.kind",
    ],
    [{ ...relationship, relationship: "extends" }, "relationship"],
    [
      {
        ...relationship,
        target: { ...relationship.target, relationshipKind: "extends" },
      },
      "target.relationshipKind",
    ],
    [
      {
        ...relationship,
        target: { ...relationship.target, kind: "overrides" },
      },
      "target.kind",
    ],
  ])
    failure("TypeRelationshipFact", value, `TypeRelationshipFact.${field}`);
  const missing = { ...relationship };
  delete missing.relationshipKind;
  failure(
    "TypeRelationshipFact",
    missing,
    "TypeRelationshipFact.relationshipKind",
  );
  assert.throws(
    () =>
      parseJson(
        '{"kind":"typeRelationship","relationshipKind":"extends","relationshipKind":"overrides"}',
      ),
    /duplicate key relationshipKind/,
  );
});
test("RELATIONSHIP_FORMAT.NESTED: captured relationship facts enforce source and closed enum", async () => {
  const relationship = populated("TypeRelationshipFact");
  const rows = [];
  for (const [type, make, field] of [
    ["Fact", (fact) => fact, "Fact"],
    [
      "AnnotationFile",
      (fact) => ({ ...sample("AnnotationFile"), facts: [fact] }),
      "AnnotationFile.facts[0]",
    ],
    [
      "SemanticCapture",
      (fact) => ({ ...sample("SemanticCapture"), facts: [fact] }),
      "SemanticCapture.facts[0]",
    ],
  ]) {
    for (const relationshipKind of ["extends", "implements", "overrides"])
      validate(type, make({ ...relationship, relationshipKind }));
    const baseline = () => make(structuredClone(relationship));
    const cases = [
      [
        "missingKind",
        (fact) => {
          delete fact.kind;
        },
        `${field}.kind`,
      ],
      [
        "unknownKind",
        (fact) => {
          fact.kind = "__unknown__";
        },
        `${field}.kind`,
      ],
      [
        "conflictingDiscriminator",
        (fact) => {
          fact.kind = "implements";
          fact.relationshipKind = "extends";
        },
        `${field}.kind`,
      ],
      [
        "conflictingRelationshipKind",
        (fact) => {
          fact.kind = "extends";
          fact.relationshipKind = "implements";
        },
        `${field}.kind`,
      ],
      [
        "missingRelationshipKind",
        (fact) => {
          delete fact.relationshipKind;
        },
        `${field}.relationshipKind`,
      ],
      [
        "unknownRelationshipKind",
        (fact) => {
          fact.relationshipKind = "inherits";
        },
        `${field}.relationshipKind`,
      ],
      [
        "discriminatorAsRelationshipKind",
        (fact) => {
          fact.relationshipKind = "typeRelationship";
        },
        `${field}.relationshipKind`,
      ],
      [
        "legacy",
        (fact) => {
          fact.relationship = "extends";
        },
        `${field}.relationship`,
      ],
      [
        "nestedLegacy",
        (fact) => {
          fact.target.relationshipKind = "extends";
        },
        `${field}.target.relationshipKind`,
      ],
      [
        "nestedConflict",
        (fact) => {
          fact.target.kind = "overrides";
        },
        `${field}.target.kind`,
      ],
      [
        "externalSource",
        (fact) => {
          fact.source = { kind: "external", symbol: sample("SymbolKey") };
        },
        `${field}.source.kind`,
      ],
    ];
    for (const [name, change, expectedField] of cases)
      rows.push({
        id: `RELATIONSHIP_FORMAT.${type}.${name}`,
        baseline,
        check: (value) => validate(type, value),
        mutate: (value) => {
          const fact = type === "Fact" ? value : value.facts[0];
          change(fact);
          return value;
        },
        expectedAssertion: "FORMAT.SHAPE",
        expectedCode: "invalidRecord",
        expectedField,
      });
  }
  registerControls(rows);
  for (const row of rows) await runControl(row);
});
test("FORMAT.ENVELOPES: comparison, diagnostics, requested limits and exclusive answers", () => {
  const fixture = sample("FixtureV1");
  validate("FixtureV1", fixture);
  for (const field of ["comparison", "coverageIntents", "anchorCasesFile"]) {
    const v = { ...fixture };
    delete v[field];
    failure("FixtureV1", v, `FixtureV1.${field}`);
  }
  const records = sample("NormalizedRecordsV1");
  validate("NormalizedRecordsV1", records);
  for (const field of ["comparison", "referenceJoinDiagnostics"]) {
    const v = { ...records };
    delete v[field];
    failure("NormalizedRecordsV1", v, `NormalizedRecordsV1.${field}`);
  }
  const requested = sample("AttemptedGraphRequest");
  requested.depth = 6;
  requested.maxNodes = 0;
  requested.maxCalls = 501;
  validate("AttemptedGraphRequest", requested);
  failure("GraphRequest", requested, "GraphRequest.request");
  for (const extra of ["cursor", "pagination", "filter"])
    failure(
      "GraphRequest",
      { ...sample("GraphRequest"), [extra]: 1 },
      `GraphRequest.${extra}`,
    );
  validate("AnswersInputV1", sample("AnswersInputV1"));
  const bad = sample("Failure");
  failure(
    "Failure",
    { ...bad, result: sample("GraphResult") },
    "Failure.result",
  );
  const diagnostic = sample("ReferenceJoinDiagnostic");
  diagnostic.join.status = "exact";
  failure(
    "ReferenceJoinDiagnostic",
    diagnostic,
    "ReferenceJoinDiagnostic.join.status",
  );
});
test("CANONICAL.INTAKE: duplicate-safe strict Unicode and integer tokens", () => {
  for (const bad of [
    '{"a":1,"a":2}',
    '{"a":1,"\u0061":2}',
    "-0",
    "1.0",
    "1e0",
    "9007199254740992",
    '"\ud800"',
    '"\udc00"',
    '{"a":1,}',
  ])
    assert.throws(() => parseJson(bad), /JSON\.INTAKE /);
  assert.throws(() => parseJson(Buffer.from([0xff])), /encoded|UTF-8/i);
  assert.deepEqual(
    { ...parseJson('{"a":[0,true,null,"é"]}') },
    { a: [0, true, null, "é"] },
  );
  assert.equal(
    canonicalBytes({
      z: '\n\t\0"\\\u2028\u2029',
      a: [0, true, null],
    }).toString(),
    '{"a":[0,true,null],"z":"\\u000a\\u0009\\u0000\\"\\\\\u2028\u2029"}',
  );
  for (const value of [
    -0,
    1.5,
    Number.MAX_SAFE_INTEGER + 1,
    "\ud800",
    { x: undefined },
  ])
    assert.throws(() => canonicalBytes(value), /JSON\.CANONICAL /);
});

// A populated specimen reaches nested policies which empty-envelope checks cannot reach.
function populated(spec, trail = [], alternatives = false) {
  if (typeof spec === "string" && Object.hasOwn(schemas, spec))
    return populated(schemas[spec], [...trail, spec], alternatives);
  if (typeof spec === "string") return sample(spec);
  if (spec.nullable) return populated(spec.nullable, trail, alternatives);
  if (spec.either)
    return populated(
      spec.either[alternatives ? spec.either.length - 1 : 0],
      trail,
      alternatives,
    );
  if (Object.hasOwn(spec, "literal")) return spec.literal;
  if (spec.enum) return spec.enum[0];
  if (spec.array) return [populated(spec.array, trail, alternatives)];
  if (spec.union)
    return populated(
      Object.values(spec.union.variants)[
        alternatives ? Object.keys(spec.union.variants).length - 1 : 0
      ],
      trail,
      alternatives,
    );
  if (spec.object)
    return Object.fromEntries(
      Object.entries(spec.object).map(([field, child]) => [
        field,
        populated(child, trail, alternatives),
      ]),
    );
  throw Error(`unknown schema ${String(spec)}`);
}
function checkShape(type, value, field) {
  assert.throws(
    () => validate(type, value),
    (error) => {
      assert.equal(error.assertion, "FORMAT.SHAPE");
      assert.equal(error.code, "invalidRecord");
      assert.equal(error.field, field);
      return true;
    },
  );
}
function nestedPolicies(type, spec, value, path, root, seen = new Set()) {
  if (typeof spec === "string" && Object.hasOwn(schemas, spec)) {
    if (seen.has(spec)) return;
    nestedPolicies(
      type,
      schemas[spec],
      value,
      path,
      root,
      new Set([...seen, spec]),
    );
    return;
  }
  if (typeof spec === "string") return;
  if (spec.nullable) {
    nestedPolicies(type, spec.nullable, value, path, root, seen);
    return;
  }
  if (spec.either) return; // Slot alternatives are checked separately below.
  if (spec.array) {
    for (let i = 0; i < value.length; i++)
      nestedPolicies(type, spec.array, value[i], `${path}[${i}]`, root, seen);
    return;
  }
  if (spec.union) {
    const variant = spec.union.variants[String(value[spec.union.tag])];
    nestedPolicies(type, variant, value, path, root, seen);
    return;
  }
  if (!spec.object) return;
  for (const [field, child] of Object.entries(spec.object)) {
    const prefix = `${path}.${field}`;
    const bad = structuredClone(root);
    // Walk the precise path from the root, including populated array elements.
    const steps = prefix.slice(type.length).match(/\.[^.[\]]+|\[\d+\]/g) ?? [];
    let parent = bad;
    for (const step of steps.slice(0, -1))
      parent =
        parent[
          step.startsWith("[") ? Number(step.slice(1, -1)) : step.slice(1)
        ];
    delete parent[field];
    checkShape(type, bad, prefix);
    if (!child?.nullable) {
      const wrong = structuredClone(root);
      let changed = wrong;
      for (const step of steps.slice(0, -1))
        changed =
          changed[
            step.startsWith("[") ? Number(step.slice(1, -1)) : step.slice(1)
          ];
      changed[field] =
        typeof value[field] === "string"
          ? 123
          : typeof value[field] === "number"
            ? "wrong"
            : typeof value[field] === "boolean"
              ? "wrong"
              : false;
      checkShape(type, wrong, prefix);
    }
    nestedPolicies(type, child, value[field], prefix, root, seen);
  }
  const bad = structuredClone(root),
    steps = path.slice(type.length).match(/\.[^.[\]]+|\[\d+\]/g) ?? [];
  let target = bad;
  for (const step of steps)
    target =
      target[step.startsWith("[") ? Number(step.slice(1, -1)) : step.slice(1)];
  target.unexpected = true;
  checkShape(type, bad, `${path}.unexpected`);
}
test("FORMAT.POPULATED: all named shapes reject nested omission and unknown keys", () => {
  for (const [type, spec] of Object.entries(schemas)) {
    const specimen = populated(type);
    validate(type, specimen);
    nestedPolicies(type, spec, specimen, type, specimen);
    // Exercise the non-null branch of nullable fields and an alternate union/typed-ref branch.
    const alternate = populated(type, [], true);
    if (type === "TypeRelationshipFact")
      alternate.source = populated("InternalTargetRef");
    validate(type, alternate);
    if (spec.object)
      for (const [field, child] of Object.entries(spec.object)) {
        if (child?.nullable) {
          const nullable = structuredClone(specimen);
          nullable[field] = null;
          validate(type, nullable);
        }
        if (child?.array) {
          const sparse = structuredClone(specimen);
          sparse[field] = Array(1);
          checkShape(type, sparse, `${type}.${field}[0]`);
        }
      }
  }
});
test("FORMAT.TYPED_REFS: only declared identity and record slots accept their own refs", () => {
  const identity = { ref: "declaration-a" },
    record = { recordRef: "call-a" };
  for (const [type, field, allowed, wrong] of [
    ["GraphRequestTemplate", "rootSyntaxId", identity, record],
    ["GraphNodeTemplate", "declaration", record, identity],
    ["GraphEdgeTemplate", "call", record, identity],
    ["AnchorResultTemplate", "targetId", identity, record],
  ]) {
    const base = populated(type);
    validate(type, { ...base, [field]: allowed });
    checkShape(type, { ...base, [field]: wrong }, `${type}.${field}`);
  }
  const internal = {
    kind: "internal",
    declarationRef: "declaration-a",
    revisionId: "revision-a",
  };
  const external = { kind: "external", symbol: populated("SymbolKey") };
  for (const [type, field] of [
    ["ReferenceEvidenceTemplate", "declaredTarget"],
    ["CallBindingEvidenceTemplate", "declaredTarget"],
  ]) {
    const base = populated(type);
    for (const target of [internal, external])
      validate(type, { ...base, [field]: target, candidates: [target] });
    checkShape(
      type,
      { ...base, [field]: { ...internal, document: populated("DocumentKey") } },
      `${type}.${field}.document`,
    );
    checkShape(
      type,
      {
        ...base,
        [field]: {
          kind: "internal",
          syntaxId: sample("SyntaxId"),
          document: populated("DocumentKey"),
          revisionId: "revision-a",
        },
      },
      `${type}.${field}.declarationRef`,
    );
  }
  validate("SymbolTemplate", {
    ...populated("SymbolTemplate"),
    declarations: [internal, external],
  });
  validate("CallBindingTemplate", {
    ...populated("CallBindingTemplate"),
    possibleDispatch: [internal, external],
  });
  validate("TargetTemplate", internal);
  checkShape("Target", internal, "Target.syntaxId");
  checkShape(
    "TargetRef",
    { ...internal, syntaxId: sample("SyntaxId") },
    "TargetRef.syntaxId",
  );
});
test("FORMAT.ENVELOPES.POPULATED: authored and published request policy stays distinct", () => {
  for (const type of [
    "FixtureV1",
    "AnnotationFile",
    "AnswerCase",
    "AnswersInputV1",
    "AnswersV1",
    "ManifestV1",
    "NormalizedRecordsV1",
    "ReferenceJoinDiagnostic",
  ])
    validate(type, populated(type));
  const attempted = {
    ...populated("AttemptedGraphRequest"),
    depth: 6,
    maxNodes: 0,
    maxCalls: 501,
  };
  const failureAnswer = populated("Failure");
  validate("AnswersV1", {
    formatVersion: 1,
    answers: [
      { id: "case", attemptedRequest: attempted, answer: failureAnswer },
    ],
  });
  validate("AnswersInputV1", {
    formatVersion: 1,
    answers: [
      {
        id: "case",
        attemptedRequest: { ...attempted, rootSyntaxId: { ref: "root" } },
        answer: failureAnswer,
      },
    ],
  });
  checkShape("GraphRequest", attempted, "GraphRequest.request");
  const success = populated("Success");
  validate("AnswersV1", {
    formatVersion: 1,
    answers: [
      {
        id: "case",
        attemptedRequest: populated("AttemptedGraphRequest"),
        answer: success,
      },
    ],
  });
  checkShape(
    "AnswersV1",
    {
      formatVersion: 1,
      answers: [
        {
          id: "case",
          attemptedRequest: attempted,
          answer: {
            ...success,
            result: { ...success.result, request: attempted },
          },
        },
      ],
    },
    "AnswersV1.answers[0].answer.result.request.request",
  );
  checkShape(
    "ManifestV1",
    {
      ...populated("ManifestV1"),
      records: { ...populated("FileDigest"), path: "/absolute" },
    },
    "ManifestV1.records.path",
  );
  checkShape(
    "DocumentKey",
    { ...populated("DocumentKey"), path: "src/../a" },
    "DocumentKey.path",
  );
  checkShape(
    "DocumentKey",
    { ...populated("DocumentKey"), sourceSetId: "\ud800" },
    "DocumentKey.sourceSetId",
  );
});
test("CANONICAL.INTAKE: only JSON whitespace, no BOM, no sparse arrays", () => {
  for (const value of ["\ufeff{}", "\u00a0{}", "{\u2003}", "[1,\u00a0 2]"])
    assert.throws(() => parseJson(value), /JSON.INTAKE/);
  assert.throws(
    () => parseJson(Buffer.from([0xef, 0xbb, 0xbf, 0x7b, 0x7d])),
    /BOM/,
  );
  assert.deepEqual({ ...parseJson(" \t\r\n{}") }, {});
  for (const value of [Array(1), [, 2], { nested: [[1, , 3]] }])
    assert.throws(() => canonicalBytes(value), /sparse array/);
  for (const value of [[1, 2], { nested: [{ x: "é" }, true, null] }])
    assert.deepEqual(
      JSON.parse(canonicalBytes(value).toString()),
      JSON.parse(JSON.stringify(value)),
    );
  checkShape(
    "SourceManifestInput",
    [, populated("SourceManifestRow")],
    "SourceManifestInput[0]",
  );
});
test("MUTATION.REGISTRY: baseline is checked before mutation and IDs remain enumerable", async () => {
  const id = "FORMAT.REGISTRY.local-one";
  const row = {
    id,
    baseline: () => ({ value: 1 }),
    check: (v) => assert.equal(v.value, 1),
    mutate: (v) => v,
    expectedAssertion: "FORMAT.SHAPE",
    expectedCode: "invalidRecord",
    expectedField: "value",
  };
  registerControls([row]);
  assert.ok(controls.some((control) => control.id === id));
  assert.throws(() => registerControls([{ ...row }]), /Duplicate control/);
  assert.throws(
    () =>
      registerControls([
        { ...row, id: "FORMAT.REGISTRY.no-check", check: null },
      ]),
    /requires baseline, check and mutate functions/,
  );
  const second = { ...row, id: "FORMAT.REGISTRY.local-two" };
  registerControls([second]);
  assert.ok(controls.some((control) => control.id === second.id));
  let mutated = false;
  await assert.rejects(
    runControl({
      ...row,
      id: "FORMAT.REGISTRY.bad-baseline",
      baseline: () => ({}),
      check: (v) => {
        assert.equal(v.value, 1);
      },
      mutate: (v) => {
        mutated = true;
        return v;
      },
    }),
    /undefined !== 1/,
  );
  assert.equal(mutated, false);
});

test("FORMAT.UNIONS.POPULATED: each nested union variant enforces closed fields", () => {
  for (const [type, spec] of Object.entries(schemas)) {
    if (!spec.union) continue;
    for (const variant of Object.values(spec.union.variants)) {
      const value = populated(variant);
      validate(type, value);
      nestedPolicies(type, variant, value, type, value);
    }
  }
});

// Each local alternative is installed in a complete, populated root envelope. The
// registry records every negative, including nested element and typed-ref controls.
function at(root, steps, value) {
  const copy = structuredClone(root);
  if (!steps.length) return structuredClone(value);
  let node = copy;
  for (const step of steps.slice(0, -1)) node = node[step];
  node[steps.at(-1)] = structuredClone(value);
  return copy;
}
function pathOf(type, steps) {
  return (
    type +
    steps
      .map((step) => (typeof step === "number" ? `[${step}]` : `.${step}`))
      .join("")
  );
}
function choice(spec) {
  return populated(spec);
}
function shapeError(type, value, field) {
  try {
    validate(type, value);
    return false;
  } catch (error) {
    return (
      error.assertion === "FORMAT.SHAPE" &&
      error.code === "invalidRecord" &&
      error.field === field
    );
  }
}
test("FORMAT.SCHEMA_MATRIX: populated alternatives and registered precise negative controls", async () => {
  const rows = [];
  const covered = [];
  function positive(type, root, steps, candidate, label) {
    const changed = at(root, steps, candidate);
    validate(type, changed);
    covered.push(`${type}:${pathOf(type, steps)}:${label}`);
    return changed;
  }
  function negative(type, root, steps, bad, field, label) {
    const id = `FORMAT.MATRIX.${type}.${steps.map(String).join(".") || "root"}.${label}`;
    rows.push({
      id,
      baseline: () => root,
      check: (value) => validate(type, value),
      mutate: (value) => at(value, steps, bad),
      expectedAssertion: "FORMAT.SHAPE",
      expectedCode: "invalidRecord",
      expectedField: field,
    });
  }
  function wrong(type, root, steps, field, label, valid) {
    for (const candidate of [false, 123, "wrong", {}, [], null]) {
      if (candidate === null && valid?.nullable) continue;
      if (shapeError(type, at(root, steps, candidate), field)) {
        negative(type, root, steps, candidate, field, label);
        return;
      }
    }
    throw Error(`No wrong value for ${type} ${field} ${label}`);
  }
  function visit(
    type,
    spec,
    root,
    steps,
    branch = "primary",
    opaqueSlot = null,
  ) {
    const path = pathOf(type, steps);
    // An either rejects failed alternatives at its slot, not at the inner field.
    const diagnostic = (field) => opaqueSlot ?? field;
    if (typeof spec === "string" && Object.hasOwn(schemas, spec))
      return visit(type, schemas[spec], root, steps, branch, opaqueSlot);
    if (typeof spec === "string") return;
    if (spec.nullable) {
      positive(type, root, steps, null, `${branch}.null`);
      const nonnull = positive(
        type,
        root,
        steps,
        choice(spec.nullable),
        `${branch}.nonnull`,
      );
      wrong(
        type,
        nonnull,
        steps,
        diagnostic(path),
        `${branch}.nullableWrong`,
        spec,
      );
      visit(
        type,
        spec.nullable,
        nonnull,
        steps,
        `${branch}.nonnull`,
        opaqueSlot,
      );
      return;
    }
    if (spec.either) {
      for (let i = 0; i < spec.either.length; i++) {
        const base = positive(
          type,
          root,
          steps,
          choice(spec.either[i]),
          `${branch}.either${i}`,
        );
        visit(
          type,
          spec.either[i],
          base,
          steps,
          `${branch}.either${i}`,
          opaqueSlot ?? path,
        );
      }
      if (
        spec.either.includes("IdentityRef") ||
        spec.either.includes("RecordRef")
      ) {
        const correct = spec.either.includes("IdentityRef")
          ? { ref: "identity" }
          : { recordRef: "record" };
        const incorrect = spec.either.includes("IdentityRef")
          ? { recordRef: "record" }
          : { ref: "identity" };
        const base = positive(type, root, steps, correct, `${branch}.typedRef`);
        negative(
          type,
          base,
          steps,
          incorrect,
          diagnostic(path),
          `${branch}.crossKindRef`,
        );
      }
      return;
    }
    if (spec.enum) {
      for (const [i, value] of spec.enum.entries())
        positive(type, root, steps, value, `${branch}.enum${i}`);
      wrong(type, root, steps, diagnostic(path), `${branch}.enumWrong`, spec);
      return;
    }
    if (spec.union) {
      for (const [tag, variant] of Object.entries(spec.union.variants)) {
        // Relationship facts require an internal source; the external target
        // variant is covered by the target slot and standalone TargetRef.
        if (
          steps.at(-1) === "source" &&
          tag === "external" &&
          steps.slice(0, -1).reduce((v, k) => v[k], root).kind ===
            "typeRelationship"
        )
          continue;
        const base = positive(
          type,
          root,
          steps,
          choice(variant),
          `${branch}.union${tag}`,
        );
        visit(type, variant, base, steps, `${branch}.union${tag}`, opaqueSlot);
      }
      const bad = {
        ...choice(Object.values(spec.union.variants)[0]),
        [spec.union.tag]: "__unknown__",
      };
      negative(
        type,
        root,
        steps,
        bad,
        diagnostic(`${path}.${spec.union.tag}`),
        `${branch}.unknownVariant`,
      );
      return;
    }
    if (spec.array) {
      const base = positive(
        type,
        root,
        steps,
        [choice(spec.array)],
        `${branch}.populatedArray`,
      );
      wrong(
        type,
        base,
        [...steps, 0],
        diagnostic(`${path}[0]`),
        `${branch}.arrayElement`,
        spec.array,
      );
      visit(type, spec.array, base, [...steps, 0], branch, opaqueSlot);
      return;
    }
    if (spec.object) {
      negative(
        type,
        root,
        steps,
        { ...steps.reduce((v, k) => v[k], root), unexpected: true },
        diagnostic(`${path}.unexpected`),
        `${branch}.extra`,
      );
      for (const [field, child] of Object.entries(spec.object)) {
        const fieldSteps = [...steps, field],
          fieldPath = `${path}.${field}`;
        const omitted = structuredClone(steps.reduce((v, k) => v[k], root));
        delete omitted[field];
        negative(
          type,
          root,
          steps,
          omitted,
          diagnostic(fieldPath),
          `${branch}.${field}.missing`,
        );
        wrong(
          type,
          root,
          fieldSteps,
          diagnostic(fieldPath),
          `${branch}.${field}.wrong`,
          child,
        );
        visit(type, child, root, fieldSteps, branch, opaqueSlot);
      }
    }
  }
  for (const [type, spec] of Object.entries(schemas)) {
    const root = populated(type);
    validate(type, root);
    visit(type, spec, root, []);
  }
  registerControls(rows);
  assert.ok(covered.length > Object.keys(schemas).length);
  for (const row of rows) await runControl(row);
  assert.equal(
    rows.filter((row) => controls.includes(row)).length,
    rows.length,
  );
});

// Owner ruling 2026-11-19 (#1234): a Rust enum variant is the closed Kind `variant`; a trait
// associated type stays `alias`. Every other spelling still fails closed.
test("FORMAT.KIND: variant is a closed declaration Kind", () => {
  validate("Kind", "variant");
  validate("Kind", "alias");
  for (const kind of ["enumMember", "Variant", "associatedType"])
    assert.throws(
      () => validate("Kind", kind),
      (e) => e.assertion === "FORMAT.SHAPE",
    );
});
