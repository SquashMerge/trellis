import test from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { loadFixture } from "../load.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { materializeAnswers } from "../answers.mjs";
import { checkAnswers, checkGraphAnswer } from "../graph-check.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import { checkMeasurement } from "../record-check/measurement.mjs";
import { checkJoins } from "../record-check/joins.mjs";
import { checkRelationships } from "../record-check/relationships.mjs";
import { checkBindings } from "../record-check/bindings.mjs";
import {
  compareEnvelope,
  checkEnvelopeOrder,
  orderedEnvelope,
} from "../record-check/measurement.mjs";
import { registerControls, runControl } from "./mutations.mjs";
import { validate } from "../formats.mjs";

const root = fileURLToPath(
  new URL(
    "../../../tests/fixtures/semantic-evidence/v1/example/",
    import.meta.url,
  ),
);
const fixture = await loadFixture(root);
const normalized = normalizeFixture(fixture);
const { records } = normalized;
assert.equal(fixture.native.declarations.length, 32);
assert.ok(fixture.native.declarations.every((row) => row.role === "production"));
assert.ok(records.declarations.every((row) => row.role === "production"));
const answers = materializeAnswers(fixture.answers, normalized);
const coverage = checkCoverage(fixture, records);
const measurement = checkMeasurement(fixture, records);
const joins = checkJoins(fixture, records, coverage, measurement);
checkRelationships(fixture, records, coverage, measurement, joins);
checkBindings(fixture, records, coverage, measurement, joins);
const answer = (id) => answers.answers.find((row) => row.id === id);

// anchors.js is intentionally a classic script: sibling function declarations
// may share a name there, but exporting those siblings from an ES module is invalid.
test("example anchor snapshots parse as native JavaScript scripts; duplicate exports fail", () => {
  for (const revision of ["r0", "r1", "r2"]) {
    const file = join(root, "snapshots", revision, "src", "anchors.js");
    const source = readFileSync(file, "utf8");
    const check = spawnSync(process.execPath, ["--check", file], {
      encoding: "utf8",
    });
    assert.equal(check.status, 0, `${revision}: ${check.stderr}`);
    const script = spawnSync(
      process.execPath,
      ["--check", "--input-type=commonjs"],
      { input: source, encoding: "utf8" },
    );
    assert.equal(script.status, 0, `${revision} script: ${script.stderr}`);
    assert.match(source, /^function anchor\(\)/m);
    assert.equal(source.includes("export "), false);
  }
  const source = readFileSync(
    join(root, "snapshots", "r2", "src", "anchors.js"),
    "utf8",
  );
  const invalidModule = source.replace(
    /^function anchor\(\)/gm,
    "export function anchor()",
  );
  assert.notEqual(invalidModule, source);
  const regression = spawnSync(
    process.execPath,
    ["--check", "--input-type=module"],
    { input: invalidModule, encoding: "utf8" },
  );
  assert.notEqual(regression.status, 0);
  assert.match(
    regression.stderr,
    /Identifier 'anchor' has already been declared/,
  );
});

// Expected IDs and bytes are independently pinned from the literal source and
// #22 domain-separated canonical digest, not copied from a checker result.
// The occ:v2 call ID was independently recomputed with Python hashlib from
// unchanged.js bytes (9acebf74...), the empty-inventory JavaScript extraction
// context (6c408070...), native producer `native`/`1` and owner `same`.
test("example source bytes, immutable IDs and per-document occurrences", () => {
  assert.equal(
    records.declarations.find(
      (row) => row.revisionId === "r2" && row.name === "same",
    ).syntaxId,
    "sid:v1:4ceebb0a4e64a3950e7fe18ce2ec10d5",
  );
  assert.equal(
    records.declarations.find(
      (row) => row.revisionId === "r2" && row.name === "change",
    ).syntaxId,
    "sid:v1:f46279b6b2d3b40bda9d0f9ad9f289ab",
  );
  const unchangedCall = (revisionId) =>
    records.calls.find(
      (row) =>
        row.revisionId === revisionId &&
        row.document.path === "src/unchanged.js",
    );
  assert.equal(
    unchangedCall("r2").id,
    "occ:v2:fe3eb5ddef64ba3c8e6011f4c08258a4",
  );
  // Decision 0003: byte-identical unchanged.js keeps its occurrence ID across
  // revisions, while each record and its proof remain revision-scoped.
  assert.equal(unchangedCall("r1").id, unchangedCall("r2").id);
  assert.notEqual(
    unchangedCall("r1").provenanceId,
    unchangedCall("r2").provenanceId,
  );
  assert.ok(records.calls.every((row) => !row.id.startsWith("occ:v1:")));
  assert.equal(
    fixture.revisions
      .get(JSON.stringify(["main", "r1"]))
      .documents.find((row) => row.key.path === "src/changed.js").contentHash,
    "0321caed7db4a2341355061cdb709b00a83226792a91288a85ecdca2270149d1",
  );
  assert.equal(
    fixture.revisions
      .get(JSON.stringify(["main", "r2"]))
      .documents.find((row) => row.key.path === "src/changed.js").contentHash,
    "3d45199342e9522ccdccb92712de06e94fa823a9a4feb5c2aba7eb197a8518b2",
  );
  assert.equal(records.coverage.length, 45);
  assert.deepEqual(
    fixture.revisionChronology.get("main").map((row) => row.id),
    ["r0", "r1", "r2"],
  );
});

// Decision 0003 fail-closed precondition: no occ:v2 ID is minted (by the
// normalizer) or re-derived (by the independent checker) unless the native
// producer's declared inventory is explicit, known, sorted, unique, equal to the
// trusted inventory of the admitted producer version, and each component
// resolves to an authenticated capture of the pinned revision.
test("undeclared, unknown, duplicate or unauthenticated extraction inventory fails closed", () => {
  const variant = (mutate) => {
    const loaded = { ...fixture, native: structuredClone(fixture.native) };
    mutate(loaded);
    return loaded;
  };
  const omitted = variant((x) => delete x.native.extractionInputs);
  for (const run of [normalizeFixture, (x) => checkMeasurement(x, records)])
    assert.throws(
      () => run(omitted),
      (e) => e.assertion === "FORMAT.SHAPE" && /extractionInputs/.test(e.field),
    );
  for (const inputs of [
    ["source"],
    ["config", "config"],
    ["toolchain", "config"],
  ]) {
    const bad = variant((x) => (x.native.extractionInputs = inputs));
    if (inputs[0] === "source")
      assert.throws(() => normalizeFixture(bad), /FORMAT/);
    else
      for (const run of [normalizeFixture, (x) => checkMeasurement(x, records)])
        assert.throws(
          () => run(bad),
          (e) =>
            e.assertion === "IDENTITY.EXTRACTION_CONTEXT" &&
            e.field === "extractionInputs",
          inputs.join(),
        );
  }
  // The artifact's inventory is a claim checked against the trusted inventory
  // of the admitted producer version, never the authority.
  const withVersion = (x, version) =>
    (x.fixture = {
      ...x.fixture,
      producers: x.fixture.producers.map((p) =>
        p.id === x.native.producerId && p.kind === "native"
          ? { ...p, version }
          : p,
      ),
    });
  const rejected = (loaded, reason, message) => {
    for (const run of [normalizeFixture, (x) => checkMeasurement(x, records)])
      assert.throws(
        () => run(loaded),
        (e) =>
          e.assertion === "IDENTITY.EXTRACTION_CONTEXT" &&
          e.field === "extractionInputs" &&
          reason.test(e.message),
        message,
      );
  };
  // Same producer version re-declaring its inventory ([] -> [config]).
  rejected(
    variant((x) => (x.native.extractionInputs = ["config"])),
    /differs from the trusted inventory of native@1 /,
    "same-version [] -> [config]",
  );
  // A config-reading producer version that omits config from its declaration,
  // so a changed config would not re-identify its occurrences.
  rejected(
    variant((x) => {
      withVersion(x, "1+config");
      x.fixture.captures = x.fixture.captures.map((c) =>
        c.kind === "config" ? { ...c, hash: "0".repeat(64) } : c,
      );
    }),
    /differs from the trusted inventory of native@1\+config /,
    "changed config with omitted input",
  );
  // A producer version with no trusted inventory.
  rejected(
    variant((x) => withVersion(x, "2")),
    /no trusted inventory for native producer native@2 /,
    "unknown producer version",
  );
  // The trusted inventory matches, but its component has no authenticated capture.
  rejected(
    variant((x) => {
      withVersion(x, "1+config");
      x.native.extractionInputs = ["config"];
      x.fixture.captures = x.fixture.captures.filter(
        (c) => c.kind !== "config",
      );
    }),
    /component config lacks an authenticated capture/,
    "unauthenticated capture",
  );
  // A legitimate config-reading producer version is a different context, so it
  // re-identifies every occurrence; the published [] records then fail.
  const config = variant((x) => {
    withVersion(x, "1+config");
    x.native.extractionInputs = ["config"];
  });
  const reidentified = normalizeFixture(config).records;
  assert.equal(reidentified.calls.length, records.calls.length);
  for (const [i, row] of reidentified.calls.entries())
    assert.notEqual(row.id, records.calls[i].id);
  assert.throws(
    () => checkMeasurement(config, records),
    (e) => e.assertion === "RECORDS.MEMBERSHIP",
  );
});

// Owner ruling on Decision 0003: rows published under the withdrawn
// revision-bound occ:v1 identity must not pass the occ:v2 checker, neither with
// their literal v1 spelling nor with the v1 digest re-labelled as occ:v2.
test("withdrawn occ:v1 rows and v1-derived values are rejected by the occ:v2 checker", () => {
  // Formerly published r2 unchanged.js call (v1 bundle 676d58ad...).
  const v1 = "56fac677947ade31ace83067fd89c569";
  const call = (rows) =>
    rows.calls.find(
      (row) =>
        row.revisionId === "r2" && row.document.path === "src/unchanged.js",
    );
  assert.equal(validate("Call", call(records)), undefined);
  for (const [id, wrongSpelling] of [
    [`occ:v1:${v1}`, true],
    [`occ:v2:${v1}`, false],
  ]) {
    const mutated = structuredClone(records);
    Object.assign(call(mutated), { id, provenanceId: `native:r2:${id}` });
    mutated.calls = orderedEnvelope("calls", mutated.calls);
    if (wrongSpelling)
      assert.throws(
        () => validate("Call", call(mutated)),
        (e) => e.assertion === "FORMAT.SHAPE" && e.field === "Call.id",
      );
    else validate("Call", call(mutated));
    assert.throws(
      () => checkMeasurement(fixture, mutated),
      (e) => e.assertion === "RECORDS.MEMBERSHIP" && e.field === "calls",
      id,
    );
  }
});

test("authored graph answers select latest declaration proofs, never old occurrences", () => {
  for (const entry of answers.answers)
    assert.equal(
      checkGraphAnswer(fixture, records, coverage, entry),
      true,
      entry.id,
    );
  const result = (id) => answer(id).answer.result;
  assert.deepEqual(
    result("history-unchanged")
      .provenance.filter((p) => p.producerId === "P")
      .map((p) => p.id),
    ["proof:latest-unchanged", "proof:symbol-unchanged"],
  );
  assert.deepEqual(
    result("history-changed")
      .provenance.filter((p) => p.producerId === "P")
      .map((p) => [p.id, p.freshness]),
    [
      ["proof:latest-changed", "stale"],
      ["proof:relationship-changed", "stale"],
    ],
  );
  assert.equal(
    result("history-unchanged").provenance.find(
      (p) => p.id === "proof:latest-unchanged",
    ).freshness,
    "possiblyStale",
  );
  assert.equal(
    result("history-zero").provenance.some((p) => p.id === "proof:old-zero"),
    false,
  );
  assert.deepEqual(
    result("history-zero")
      .coverage.filter((c) => c.producerId === "P")
      .map((c) => [c.revisionId, c.state]),
    [
      ["r1", "partial"],
      ["r2", "failed"],
    ],
  );
  assert.equal(
    result("history-foreign").warnings.some(
      (w) => w.code === "coverageIncomplete",
    ),
    false,
  );
  assert.equal(result("history-unchanged").edges[0].binding, null);
  assert.equal(
    records.references.some((ref) => ref.revisionId === "r2"),
    false,
  );
  assert.deepEqual(
    records.callBindings
      .map((binding) => binding.join.anchor.revisionId)
      .sort(),
    ["r1", "r2"],
  );
  assert.deepEqual(
    result("history-foreign").nodes.map((node) => node.declaration.syntaxId),
    [
      "sid:v1:c3675e9d70639d85c68167b265ef5c2f",
      "sid:v1:1f8d90bd409d6d82fe4d5acb0f819a09",
    ],
  );
  assert.equal(
    result("history-foreign").edges[0].to,
    "sid:v1:1f8d90bd409d6d82fe4d5acb0f819a09",
  );
  assert.equal(result("history-foreign").edges[0].visit, "new");
  assert.equal(
    result("history-foreign").edges[0].binding.provenanceId,
    "proof:fresh-call",
  );
  assert.deepEqual(
    result("history-foreign").coverage.map((c) => [
      c.producerId,
      c.revisionId,
      c.state,
    ]),
    [
      ["P", "r2", "complete"],
      ["native", "r2", "complete"],
    ],
  );
  assert.deepEqual(result("history-foreign").warnings, []);
  assert.equal(
    result("history-other-producer").provenance.some(
      (p) => p.producerId === "Q",
    ),
    false,
  );
  assert.equal(
    records.provenance.some(
      (p) =>
        p.id === "proof:Q-anchor" &&
        p.producerId === "Q" &&
        p.revisionId === "r1",
    ),
    true,
  );
  assert.equal(
    records.declarationBindings.find((b) => b.provenanceId === "proof:Q-anchor")
      .syntaxId,
    result("history-other-producer").nodes[0].declaration.syntaxId,
  );
  assert.equal(
    records.provenance.find((p) => p.id === "proof:Q-anchor").document.path,
    "src/anchors.js",
  );
  assert.equal(
    records.coverage.some(
      (c) =>
        c.producerId === "Q" &&
        c.documentPath === "src/anchors.js" &&
        c.revisionId === "r1" &&
        c.selected &&
        c.state === "complete",
    ),
    true,
  );
  assert.deepEqual(
    result("history-other-producer").coverage.map((c) => [
      c.producerId,
      c.revisionId,
      c.state,
    ]),
    [
      ["P", "r1", "complete"],
      ["P", "r2", "failed"],
      ["native", "r2", "complete"],
    ],
  );
  assert.equal(
    result("history-other-producer").warnings[0].code,
    "coverageIncomplete",
  );
  for (const id of [
    "old-call",
    "old-reference",
    "unreturned-hidden",
    "foreign-only",
  ])
    assert.equal(
      result("history-unchanged").provenance.some(
        (proof) => proof.id === `proof:${id}`,
      ),
      false,
    );
  assert.deepEqual(
    result("syntax-only").warnings.map((row) => row.code),
    ["syntaxOnly"],
  );
});

test("six authored anchor branches retain actual source declarations", () => {
  assert.deepEqual(
    fixture.anchors.cases.map((row) => [row.id, row.expectedResult.reason]),
    [
      ["unique-unchanged-header-changed-sibling", "none"],
      ["duplicate-proven", "none"],
      ["duplicate-unknown", "unprovenContinuity"],
      ["duplicate-changed", "groupChanged"],
      ["missing", "missing"],
      ["header-mismatch", "headerMismatch"],
    ],
  );
  assert.equal(records.durableAnchors.length, 6);
});

test("durable anchor order uses captured revision and document without collapsing duplicates", () => {
  const captured = records.durableAnchors[0];
  const older = { ...captured, capturedRevisionId: "r0" };
  const earlierDocument = {
    ...captured,
    document: { ...captured.document, path: "src/aaa.js" },
  };
  assert.equal(compareEnvelope(older, captured) < 0, true);
  assert.equal(compareEnvelope(captured, older) > 0, true);
  assert.equal(compareEnvelope(earlierDocument, captured) < 0, true);
  assert.deepEqual(orderedEnvelope("durableAnchors", [captured, older]), [
    older,
    captured,
  ]);
  assert.doesNotThrow(() =>
    checkEnvelopeOrder("durableAnchors", [older, captured]),
  );
  assert.throws(() => checkEnvelopeOrder("durableAnchors", [captured, older]), {
    assertion: "RECORDS.ORDER",
    code: "invalidRecord",
    field: "durableAnchors",
  });
  assert.throws(
    () =>
      orderedEnvelope("durableAnchors", [captured, captured], {
        collapseIdentical: true,
      }),
    {
      assertion: "RECORDS.MEMBERSHIP",
      code: "invalidRecord",
      field: "durableAnchors",
    },
  );
  const declaration = records.declarations.find(
    (row) => row.revisionId === "r1" && row.name === "same",
  );
  const newer = records.declarations.find(
    (row) => row.revisionId === "r2" && row.name === "same",
  );
  assert.equal(compareEnvelope(declaration, newer) < 0, true);
});

function control(id, mutate, expectedAssertion, expectedField) {
  return {
    id,
    baseline: () => answer(id.split("/")[0]),
    mutate,
    check: (entry) => checkGraphAnswer(fixture, records, coverage, entry),
    expectedAssertion,
    expectedCode: "invalidRecord",
    expectedField,
  };
}
const controls = registerControls([
  control(
    "history-zero/older-r0-fallback",
    (entry) => {
      entry.answer.result.provenance.push(
        records.provenance.find((p) => p.id === "proof:old-zero"),
      );
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-unchanged/unreturned-declaration",
    (entry) => {
      entry.answer.result.provenance.push(
        records.provenance.find((p) => p.id === "proof:unreturned-hidden"),
      );
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-unchanged/foreign-document",
    (entry) => {
      entry.answer.result.provenance.push(
        records.provenance.find((p) => p.id === "proof:foreign-only"),
      );
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-unchanged/old-call-binding",
    (entry) => {
      entry.answer.result.edges[0].binding = records.callBindings[0];
      return entry;
    },
    "GRAPH.TRAVERSAL",
    "answers.history-unchanged.result.edges",
  ),
  control(
    "history-unchanged/old-reference-proof",
    (entry) => {
      entry.answer.result.provenance.push(
        records.provenance.find((p) => p.id === "proof:old-reference"),
      );
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-other-producer/Q-proof-in-P-answer",
    (entry) => {
      entry.answer.result.provenance.push(
        records.provenance.find((p) => p.id === "proof:Q-anchor"),
      );
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-unchanged/promoted-r1-proof-as-r2",
    (entry) => {
      const proof = entry.answer.result.provenance.find(
        (p) => p.id === "proof:latest-unchanged",
      );
      Object.assign(proof, { revisionId: "r2", freshness: "fresh" });
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-zero/missing-zero-fact-coverage",
    (entry) => {
      entry.answer.result.coverage = entry.answer.result.coverage.filter(
        (row) => row.revisionId !== "r1",
      );
      return entry;
    },
    "GRAPH.COVERAGE",
    "coverage",
  ),
  control(
    "history-changed/relabelled-stale",
    (entry) => {
      entry.answer.result.provenance.find(
        (p) => p.id === "proof:latest-changed",
      ).freshness = "possiblyStale";
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-unchanged/relabelled-possibly-stale",
    (entry) => {
      entry.answer.result.provenance.find(
        (p) => p.id === "proof:latest-unchanged",
      ).freshness = "fresh";
      return entry;
    },
    "GRAPH.PROVENANCE",
    "provenance",
  ),
  control(
    "history-foreign/forbidden-coverage-warning",
    (entry) => {
      entry.answer.result.warnings.unshift({
        code: "coverageIncomplete",
        provenanceId: null,
        message: "coverageIncomplete",
      });
      return entry;
    },
    "WARNING.KEYS",
    "warnings",
  ),
]);
for (const row of controls)
  test(`negative input ${row.id} fails at ${row.expectedAssertion}`, () =>
    runControl(row));

test("complete real fixture check, including source-backed durable anchors", () => {
  assert.equal(checkAnswers(fixture, records, coverage, answers), true);
});
