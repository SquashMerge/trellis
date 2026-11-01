import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { loadFixture } from "../load.mjs";
import { normalizeFixture } from "../normalize.mjs";
import { checkAnswers, checkGraphAnswer } from "../graph-check.mjs";
import { checkCoverage } from "../record-check/coverage.mjs";
import { checkMeasurement } from "../record-check/measurement.mjs";
import { checkJoins } from "../record-check/joins.mjs";
import { checkRelationships } from "../record-check/relationships.mjs";
import { checkBindings } from "../record-check/bindings.mjs";
import { checkAnchors } from "../record-check/anchors.mjs";
import {
  contentHash,
  sourceManifestHash,
  syntaxId,
  occurrenceId,
} from "../identity.mjs";
import { canonicalBytes } from "../json.mjs";
import { registerControls, runControl } from "./mutations.mjs";

const sid = (n) => `sid:v1:${String(n).padStart(32, "0")}`;
const oid = (n) => `occ:v2:${String(n).padStart(32, "0")}`;
const D = { sourceSetId: "main", language: "javascript", path: "src/go.js" };
const coverage = (p, r, state) => ({
  producerId: p,
  sourceSetId: "main",
  language: "javascript",
  documentPath: D.path,
  revisionId: r,
  selected: state !== "omitted",
  requested: true,
  state,
  supportedRoles: [],
  observedRoles: [],
  diagnostic: state === "complete" ? null : "diagnostic",
});
function specimen({ state = "complete", old = false, changed = false } = {}) {
  const r0 = {
    id: "r0",
    sourceSetId: "main",
    documents: [{ key: D, contentHash: "same" }],
  };
  const r1 = {
    id: "r1",
    sourceSetId: "main",
    documents: [{ key: D, contentHash: "same" }],
  };
  const r2 = {
    id: "r2",
    sourceSetId: "main",
    documents: [{ key: D, contentHash: changed ? "changed" : "same" }],
  };
  const declarations = "ABCD".split("").map((name, i) => ({
    name,
    syntaxId: sid(i + 1),
    document: D,
    revisionId: "r2",
    provenanceId: `native:${name}`,
  }));
  const nativeProofs = declarations.map((d) => ({
    id: d.provenanceId,
    producerId: "N",
    document: D,
    revisionId: "r2",
    freshness: "fresh",
  }));
  const records = {
    sourceSets: [{ id: "main" }],
    revisions: [r0, r1, r2],
    producers: [
      { id: "N", kind: "native" },
      { id: "P", kind: "semantic" },
    ],
    declarations,
    calls: [],
    callBindings: [],
    references: [],
    symbols: [],
    declarationBindings: [],
    typeRelationships: [],
    coverage: [
      coverage("N", "r2", "complete"),
      coverage("P", "r2", state),
      coverage("P", "r1", "complete"),
      coverage("P", "r0", "complete"),
    ],
    provenance: nativeProofs,
  };
  records.comparison = {
    sourceSetId: "main",
    revisionId: "r2",
    producers: records.producers,
  };
  const loaded = {
    native: { producerId: "N" },
    annotations: [],
    semanticProofs: new Map(),
    revisionChronology: new Map([["main", [r0, r1, r2]]]),
  };
  const checked = {
    checkUse: ({ producerId, revisionId, provenanceIds }) => {
      for (const id of provenanceIds)
        assert.ok(
          records.provenance.some(
            (p) =>
              p.id === id &&
              p.producerId === producerId &&
              p.revisionId === revisionId,
          ),
        );
    },
  };
  const request = {
    sourceSetId: "main",
    revisionId: "r2",
    rootSyntaxId: sid(1),
    semanticProducerId: "P",
    depth: 2,
    maxNodes: 150,
    maxCalls: 500,
    includeExternal: false,
    includeTests: false,
  };
  let counter = 0;
  function add(
    from,
    to,
    {
      start = counter * 10,
      dispatch = "direct",
      resolution = "resolved",
      freshness = "fresh",
      staleTarget = false,
      oldBinding = false,
      possibleDispatch = [],
      targetRevision = "r2",
    } = {},
  ) {
    const id = oid(++counter),
      call = {
        id,
        ownerSyntaxId: sid(from),
        ordinal: counter,
        document: D,
        revisionId: "r2",
        range: { start, end: start + 4 },
        provenanceId: `native:call:${counter}`,
      };
    records.calls.push(call);
    records.provenance.push({
      id: call.provenanceId,
      producerId: "N",
      document: D,
      revisionId: "r2",
      freshness: "fresh",
    });
    const proofId = `P:${counter}`,
      rev = oldBinding ? "r1" : "r2";
    const binding = {
      callId: id,
      join: {
        status: "exact",
        candidateIds: [id],
        anchor: {
          document: D,
          revisionId: rev,
          contentHash: changed ? "changed" : "same",
        },
      },
      resolution,
      declaredTarget:
        resolution === "resolved"
          ? {
              kind: "internal",
              syntaxId: sid(to),
              document: D,
              revisionId: targetRevision,
            }
          : null,
      candidates: [],
      dispatch,
      possibleDispatch: possibleDispatch.map((n) => ({
        kind: "internal",
        syntaxId: sid(n),
        document: D,
        revisionId: "r2",
      })),
      possibleDispatchComplete: false,
      staleTarget,
      provenanceId: proofId,
    };
    records.callBindings.push(binding);
    records.provenance.push({
      id: proofId,
      producerId: "P",
      document: D,
      revisionId: rev,
      contentHash: changed ? "changed" : "same",
      freshness,
    });
    return { call, binding };
  }
  // Every expected output below is built from stated IDs/rows, not expectedGraph.
  const authored = ({
    names = ["A"],
    depths = names.map(() => 0),
    edges = [],
    frontier = [],
    proofIds = [],
    extraCoverage = [],
    warnings = [],
    partial = false,
    truncated = false,
    req = request,
  } = {}) => ({
    id: "case",
    attemptedRequest: req,
    answer: {
      ok: true,
      result: {
        request: req,
        resolvedRevisionId: "r2",
        nodes: names.map((name, i) => ({
          declaration: declarations["ABCD".indexOf(name)],
          depth: depths[i],
        })),
        edges,
        frontier,
        omitted: Object.fromEntries(["filtered", "callLimit", "responseBytes", "depth"].map((reason) =>
          [reason, frontier.filter((f) => f.reason === reason).reduce((sum, f) => sum + f.omittedCalls, 0)])),
        hints: [
          ...(frontier.some((f) => f.reason === "depth") && req.depth < 5 ? ["depth"] : []),
          ...(frontier.some((f) => f.reason === "callLimit") && req.maxCalls < 500 ? ["maxCalls"] : []),
          ...(frontier.some((f) => f.reason === "nodeLimit") && req.maxNodes < 150 ? ["maxNodes"] : []),
          ...(truncated ? ["rootSyntaxId"] : []),
        ].sort(),
        coverage: [
          records.coverage[0],
          ...extraCoverage,
          ...(req.semanticProducerId === null ? [] : [records.coverage[1]]),
        ].sort(
          (a, b) =>
            a.producerId.localeCompare(b.producerId) ||
            a.revisionId.localeCompare(b.revisionId),
        ),
        provenance: records.provenance
          .filter((p) =>
            [
              ...names.map((name) => `native:${name}`),
              ...edges.map((e) => e.call.provenanceId),
              ...proofIds,
            ].includes(p.id),
          )
          .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id))),
        partial,
        truncated,
        warnings,
      },
    },
  });
  const edge = (
    pair,
    from,
    {
      reason = "none",
      visit = "new",
      to = pair.binding.declaredTarget?.syntaxId ?? null,
      binding = pair.binding,
    } = {},
  ) => ({
    call: pair.call,
    callSiteCount: 1,
    from: sid(from),
    to: ["none", "dispatch"].includes(reason) ? to : null,
    binding,
    visit: reason === "none" ? visit : "boundary",
    boundaryReason: reason,
  });
  const front = (
    reason,
    node,
    callId = null,
    targetId = null,
    nextOrdinal = null,
    omittedCalls = 0,
  ) => ({
    reason,
    nodeId: sid(node),
    callId,
    targetId,
    nextOrdinal: reason === "callLimit" ? null : nextOrdinal,
    omittedCalls,
  });
  return { records, loaded, checked, request, authored, add, edge, front };
}
const verify = (s, entry) =>
  checkGraphAnswer(s.loaded, s.records, s.checked, entry);

test("eight hand-authored priority, cycle, diamond, callback, depth, node and call cap cases", () => {
  {
    const s = specimen();
    assert.equal(verify(s, s.authored()), true);
  }
  {
    const s = specimen();
    const b = s.add(1, 2, { start: 20 }),
      c = s.add(1, 3, { start: 1 }),
      d = s.add(3, 4),
      diamond = s.add(2, 4),
      cycle = s.add(4, 1);
    const e = s.authored({
      names: ["A", "C", "B", "D"],
      depths: [0, 1, 1, 2],
      edges: [
        s.edge(c, 1),
        s.edge(b, 1),
        s.edge(diamond, 2),
        s.edge(d, 3, { visit: "seen" }),
        s.edge(cycle, 4, { visit: "seen" }),
      ],
      proofIds: [
        c.binding.provenanceId,
        b.binding.provenanceId,
        d.binding.provenanceId,
        diamond.binding.provenanceId,
        cycle.binding.provenanceId,
      ],
    });
    e.attemptedRequest.depth = e.answer.result.request.depth = 3;
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    const loop = s.add(1, 1);
    s.records.references.push({
      ownerSyntaxId: sid(1),
      declaredTarget: { kind: "internal", syntaxId: sid(4) },
    });
    assert.equal(
      verify(
        s,
        s.authored({
          edges: [s.edge(loop, 1, { visit: "seen" })],
          proofIds: [loop.binding.provenanceId],
        }),
      ),
      true,
    );
  }
  {
    const s = specimen();
    const b = s.add(1, 2),
      c = s.add(2, 3);
    const e = s.authored({
      names: ["A", "B"],
      depths: [0, 1],
      edges: [s.edge(b, 1)],
      proofIds: [b.binding.provenanceId],
      frontier: [s.front("depth", 2, null, null, 0, 1)],
      partial: true,
      truncated: true,
    });
    e.attemptedRequest.depth = e.answer.result.request.depth = 1;
    e.answer.result.hints = ["depth", "rootSyntaxId"];
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    const b = s.add(1, 2),
      c = s.add(1, 3),
      a = s.add(1, 1);
    const e = s.authored({
      names: ["A", "B"],
      depths: [0, 1],
      edges: [
        s.edge(b, 1),
        s.edge(c, 1, { reason: "nodeLimit" }),
        s.edge(a, 1, { visit: "seen" }),
      ],
      proofIds: [
        b.binding.provenanceId,
        c.binding.provenanceId,
        a.binding.provenanceId,
      ],
      frontier: [s.front("nodeLimit", 1, c.call.id, sid(3))],
      partial: true,
      truncated: true,
    });
    e.attemptedRequest.maxNodes = e.answer.result.request.maxNodes = 2;
    e.answer.result.hints = ["maxNodes", "rootSyntaxId"];
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    const b = s.add(1, 2),
      c = s.add(1, 3),
      d = s.add(2, 4);
    const e = s.authored({
      names: ["A", "B"],
      depths: [0, 1],
      edges: [s.edge(b, 1)],
      proofIds: [b.binding.provenanceId],
      frontier: [
        s.front("callLimit", 1, null, null, c.call.ordinal, 1),
        s.front("callLimit", 2, null, null, 0, 1),
      ],
      partial: true,
      truncated: true,
    });
    e.attemptedRequest.maxCalls = e.answer.result.request.maxCalls = 1;
    e.answer.result.hints = ["maxCalls", "rootSyntaxId"];
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    s.add(1, 2);
    const e = s.authored({
      frontier: [s.front("callLimit", 1, null, null, 1, 1)],
      partial: true,
      truncated: true,
    });
    e.attemptedRequest.maxCalls = e.answer.result.request.maxCalls = 0;
    if (e.answer.result.frontier.length) e.answer.result.hints = ["maxCalls", "rootSyntaxId"];
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    const dispatch = s.add(1, 2, {
        dispatch: "virtual",
        possibleDispatch: [2],
      }),
      stale = s.add(1, 2, { staleTarget: true });
    // The dispatch boundary admits its declaration, never expanded; a stale one does not (#557).
    const e = s.authored({
      names: ["A", "B"],
      depths: [0, 1],
      edges: [
        s.edge(dispatch, 1, { reason: "dispatch" }),
        s.edge(stale, 1, { reason: "stale" }),
      ],
      proofIds: [dispatch.binding.provenanceId, stale.binding.provenanceId],
      warnings: [
        {
          code: "staleTarget",
          provenanceId: stale.binding.provenanceId,
          message: "stale target",
        },
      ],
      partial: true,
    });
    assert.equal(verify(s, e), true);
  }
});

test("typed request failures preserve pinned order and no fallback", () => {
  for (const [override, code, field] of [
    [{ depth: 6 }, "invalidRequest", "depth"],
    [{ maxNodes: 0 }, "invalidRequest", "maxNodes"],
    [{ maxCalls: 501 }, "invalidRequest", "maxCalls"],
    [{ sourceSetId: "other" }, "sourceSetDenied", "sourceSetId"],
    [{ revisionId: "not-admitted" }, "revisionUnavailable", "revisionId"],
    [{ rootSyntaxId: sid(9) }, "rootMissing", "rootSyntaxId"],
    [
      { semanticProducerId: "other" },
      "producerUnavailable",
      "semanticProducerId",
    ],
  ]) {
    const s = specimen(),
      req = { ...s.request, ...override },
      entry = {
        id: "failure",
        attemptedRequest: req,
        answer: { ok: false, error: { code, field, message: "not available" } },
      };
    assert.equal(verify(s, entry), true);
  }
});

test("failed and omitted r2 keep measured calls as missingEvidence when only r1 call facts exist", () => {
  for (const state of ["failed", "omitted"])
    for (const changed of [false, true]) {
      const s = specimen({ state, changed }),
        old = s.add(1, 2, { oldBinding: true });
      const current = s.add(1, 2);
      s.records.references.push({
        revisionId: "r1",
        ownerSyntaxId: sid(1),
        provenanceId: "old-reference",
        declaredTarget: { kind: "internal", syntaxId: sid(2) },
      });
      // Only the current measured call remains. r1 call facts stay historical and
      // the current tuple contains no semantic occurrence fact at all.
      s.records.calls.shift();
      s.records.callBindings = s.records.callBindings.filter(
        (row) => row !== current.binding,
      );
      s.records.provenance = s.records.provenance.filter(
        (row) =>
          row.id !== old.call.provenanceId &&
          row.id !== current.binding.provenanceId,
      );
      const warning =
        state === "failed"
          ? [
              {
                code: "coverageIncomplete",
                provenanceId: null,
                message: "refresh failed",
              },
            ]
          : [];
      const entry = s.authored({
        edges: [
          s.edge(current, 1, { reason: "missingEvidence", binding: null }),
        ],
        extraCoverage: [s.records.coverage[2]],
        partial: true,
        warnings: warning,
      });
      assert.equal(verify(s, entry), true);
      assert.equal(entry.answer.result.edges[0].binding, null);
      assert.equal(
        entry.answer.result.provenance.some(
          (row) =>
            row.id === old.binding.provenanceId ||
            row.id === "old-reference" ||
            row.id === current.binding.provenanceId,
        ),
        false,
      );
    }
});

test("graph checker negative controls compare exact fields, proof and warning keys", async (t) => {
  const s = specimen(),
    b = s.add(1, 2),
    base = s.authored({
      names: ["A", "B"],
      depths: [0, 1],
      edges: [s.edge(b, 1)],
      proofIds: [b.binding.provenanceId],
    });
  const rows = registerControls([
    {
      id: "GRAPH.authored-edge",
      baseline: () => base,
      mutate: (x) => {
        x.answer.result.edges[0].to = sid(4);
        return x;
      },
      check: (x) => verify(s, x),
      expectedAssertion: "GRAPH.TRAVERSAL",
      expectedCode: "invalidRecord",
      expectedField: "answers.case.result.edges",
    },
    {
      id: "GRAPH.authored-proof",
      baseline: () => base,
      mutate: (x) => {
        x.answer.result.provenance.pop();
        return x;
      },
      check: (x) => verify(s, x),
      expectedAssertion: "GRAPH.PROVENANCE",
      expectedCode: "invalidRecord",
      expectedField: "provenance",
    },
    {
      id: "GRAPH.authored-warning",
      baseline: () => base,
      mutate: (x) => {
        x.answer.result.warnings.push({
          code: "syntaxOnly",
          provenanceId: null,
          message: "extra",
        });
        return x;
      },
      check: (x) => verify(s, x),
      expectedAssertion: "WARNING.KEYS",
      expectedCode: "invalidRecord",
      expectedField: "warnings",
    },
  ]);
  for (const row of rows) await t.test(row.id, () => runControl(row));
});

function historical(
  s,
  {
    revision = "r1",
    name = "A",
    kind = "declarationBinding",
    producer = "P",
    document = D,
    id = `${revision}:${kind}:${name}`,
  } = {},
) {
  const provenanceId = `proof:${id}`,
    syntaxId = sid("ABCD".indexOf(name) + 1);
  const fact =
    kind === "typeRelationship"
      ? { kind, ref: id, provenanceRef: provenanceId }
      : { kind, ref: id, record: { provenanceId } };
  const row =
    kind === "typeRelationship"
      ? {
          provenanceId,
          source: {
            kind: "internal",
            syntaxId,
            document,
            revisionId: revision,
          },
        }
      : kind === "symbol"
        ? {
            provenanceId,
            declarations: [
              { kind: "internal", syntaxId, document, revisionId: revision },
            ],
          }
        : { provenanceId, syntaxId };
  s.records[
    {
      declarationBinding: "declarationBindings",
      symbol: "symbols",
      typeRelationship: "typeRelationships",
    }[kind]
  ].push(row);
  s.records.provenance.push({
    id: provenanceId,
    producerId: producer,
    document,
    revisionId: revision,
    contentHash: "same",
    basis: { artifactHash: "captured" },
    freshness:
      s.loaded.revisionChronology.get("main")[2].documents[0].contentHash ===
      "same"
        ? "possiblyStale"
        : "stale",
  });
  s.loaded.annotations.push({ revisionId: revision, document, facts: [fact] });
  s.loaded.semanticProofs.set(provenanceId, {
    fact,
    hash: "captured",
    factRef: id,
    factKind: kind,
  });
  return provenanceId;
}
test("latest prior tuple selects only captured returned declaration proofs; changed bytes and partial warnings", () => {
  for (const changed of [false, true]) {
    const s = specimen({ state: "failed", changed }),
      old = historical(s, { revision: "r0" });
    const selected = ["declarationBinding", "symbol", "typeRelationship"].map(
      (kind) => historical(s, { kind }),
    );
    historical(s, { name: "B" });
    const req = s.request,
      entry = s.authored({
        extraCoverage: [s.records.coverage[2]],
        proofIds: selected,
        partial: true,
        warnings: [
          {
            code: "coverageIncomplete",
            provenanceId: null,
            message: "r2 failed",
          },
          {
            code: "staleEvidence",
            provenanceId: changed ? selected[0] : null,
            message: "historical proof",
          },
          ...(changed
            ? selected.slice(1).map((id) => ({
                code: "staleEvidence",
                provenanceId: id,
                message: "historical proof",
              }))
            : []),
        ],
      });
    assert.equal(verify(s, entry), true);
    assert.equal(
      entry.answer.result.provenance.some((row) => row.id === old),
      false,
    );
    assert.equal(entry.answer.result.provenance.length, 4);
  }
  const zero = specimen({ state: "failed" }),
    older = historical(zero, { revision: "r0" });
  const empty = zero.authored({
    extraCoverage: [zero.records.coverage[2]],
    partial: true,
    warnings: [
      { code: "coverageIncomplete", provenanceId: null, message: "failed" },
    ],
  });
  assert.equal(verify(zero, empty), true);
  assert.equal(
    empty.answer.result.provenance.some((p) => p.id === older),
    false,
  );
  const s = specimen({ state: "omitted" }),
    id = historical(s);
  s.records.coverage[2].state = "partial";
  s.records.coverage[2].diagnostic = "partial";
  const entry = s.authored({
    extraCoverage: [s.records.coverage[2]],
    proofIds: [id],
    partial: true,
    warnings: [
      { code: "coverageIncomplete", provenanceId: null, message: "r1 partial" },
      { code: "staleEvidence", provenanceId: null, message: "possibly stale" },
    ],
  });
  // Omitted alone is not incomplete, but the selected historical partial row is.
  assert.equal(verify(s, entry), true);
});
test("captured chain rejects relabelled and unlinked historical proofs with precise fields", async (t) => {
  const make = () => {
    const s = specimen({ state: "failed" }),
      id = historical(s);
    const entry = s.authored({
      extraCoverage: [s.records.coverage[2]],
      proofIds: [id],
      partial: true,
      warnings: [
        { code: "coverageIncomplete", provenanceId: null, message: "failed" },
        {
          code: "staleEvidence",
          provenanceId: null,
          message: "possibly stale",
        },
      ],
    });
    return { loaded: s.loaded, records: s.records, id, entry };
  };
  const check = (x) =>
    checkGraphAnswer(x.loaded, x.records, { checkUse: () => true }, x.entry);
  const controls = registerControls([
    {
      id: "GRAPH.history.relabel",
      baseline: make,
      mutate: (x) => {
        x.records.provenance.find((p) => p.id === x.id).revisionId = "r2";
        return x;
      },
      check,
      expectedAssertion: "GRAPH.HISTORY",
      expectedCode: "invalidRecord",
      expectedField: "provenance",
    },
    {
      id: "GRAPH.history.unlinked",
      baseline: make,
      mutate: (x) => {
        x.loaded.semanticProofs.delete(x.id);
        return x;
      },
      check,
      expectedAssertion: "GRAPH.HISTORY",
      expectedCode: "invalidRecord",
      expectedField: "provenance",
    },
  ]);
  for (const row of controls) await t.test(row.id, () => runControl(row));
});
// Source bytes, producer captures and annotations are admitted before the full checker runs.
// The answer below is authored from known fixture identities and expected rows, not traversal.
async function admittedGraph(
  t,
  {
    changed = false,
    state = "failed",
    zero = false,
    r2Binding = false,
    readOnlyCurrent = false,
    unsupportedCurrent = false,
    unsupportedInvocationCurrent = false,
  } = {},
) {
  const root = await mkdtemp(join(tmpdir(), "graph-history-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const files = new Map(),
    put = (path, value) =>
      files.set(
        path,
        typeof value === "string" ? value : JSON.stringify(value),
      );
  const hash = (text) => contentHash(Buffer.from(text));
  const source = "export function go() { go(); }\n",
    r2 = changed ? source + "// changed bytes\n" : source;
  const document = {
    sourceSetId: "main",
    language: "javascript",
    path: "src/go.js",
  };
  const producers = [
    ["native", "native"],
    ["semantic", "semantic"],
  ].map(([id, kind]) => ({
    id,
    kind,
    version: "1",
    executableHash: hash(`${id} executable`),
    languages: ["javascript"],
    positionEncoding: "utf8",
  }));
  const captures = [];
  function capture(ref, kind, file, bytes) {
    put(file, bytes);
    captures.push({ ref, kind, file, hash: hash(bytes) });
  }
  for (const id of ["native", "semantic"])
    capture(id, "executable", `captures/${id}.bin`, `${id} executable`);
  for (const kind of ["toolchain", "config", "dependency"])
    capture(kind, kind, `captures/${kind}.bin`, kind);
  const revisionIds = zero ? ["r0", "r1", "r2"] : ["r1", "r2"];
  const revisions = revisionIds.map((id) => ({
    id,
    sourceSetId: "main",
    documents: [
      { key: document, revisionId: id, sourceFile: `sources/${id}/go.js` },
    ],
    toolchainHash: hash("toolchain"),
    configHash: hash("config"),
    dependencyHash: hash("dependency"),
  }));
  if (zero) put("sources/r0/go.js", source);
  put("sources/r1/go.js", source);
  put("sources/r2/go.js", r2);
  const span = (start, end) => ({ encoding: "utf8", start, end }),
    name = source.indexOf("go"),
    callee = source.lastIndexOf("go");
  const header = {
    kind: "function",
    name: "go",
    modifiers: [],
    typeParameters: [],
    parameters: [],
    resultType: null,
    bases: [],
  };
  const declarations = revisionIds.map((id) => ({
    ref: `d${id}`,
    nativeId: null,
    role: "production",
    document,
    revisionId: id,
    parentRef: null,
    kind: "function",
    name: "go",
    range: span(7, source.indexOf("}") + 1),
    nameRange: span(name, name + 2),
    header,
    signature: null,
    witnesses: [
      { field: "name", witness: { range: span(name, name + 2), text: "go" } },
      {
        field: "header.name",
        witness: { range: span(name, name + 2), text: "go" },
      },
    ],
  }));
  const call = {
    ref: "r2call",
    nativeId: null,
    document,
    revisionId: "r2",
    ownerRef: "dr2",
    range: span(callee, callee + 4),
    calleeRange: unsupportedCurrent ? null : span(callee, callee + 2),
    spelling: "go",
    regionRefs: [],
    witnesses: [
      {
        field: "spelling",
        witness: { range: span(callee, callee + 2), text: "go" },
      },
    ],
  };
  const oldCall = {
    ...call,
    ref: "r1call",
    revisionId: "r1",
    ownerRef: "dr1",
    calleeRange: span(callee, callee + 2),
  };
  const oldReference = {
    ref: "r1reference",
    nativeId: null,
    document,
    revisionId: "r1",
    ownerRef: "dr1",
    range: span(callee, callee + 2),
    spelling: "go",
    witnesses: [
      {
        field: "spelling",
        witness: { range: span(callee, callee + 2), text: "go" },
      },
    ],
  };
  put("captures/native.json", {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    capabilityReports: [],
    declarations,
    calls: [oldCall, call],
    controls: [],
    references: [oldReference],
  });
  const fact = {
    kind: "symbol",
    ref: "r1symbol",
    record: {
      key: {
        scheme: "scip",
        symbol: "pkg go",
        scope: "global",
        document: null,
      },
      displayName: "go",
      declarations: [
        { kind: "internal", declarationRef: "dr1", revisionId: "r1" },
      ],
      provenanceId: "r1-proof",
    },
  };
  const olderFact = {
    ...fact,
    ref: "r0symbol",
    record: {
      ...fact.record,
      key: { ...fact.record.key, symbol: "pkg older go" },
      declarations: [
        { kind: "internal", declarationRef: "dr0", revisionId: "r0" },
      ],
      provenanceId: "r0-proof",
    },
  };
  const oldBinding = {
    kind: "callBinding",
    ref: "r1-binding",
    anchor: {
      document,
      revisionId: "r1",
      contentHash: hash(source),
      kind: "callee",
      range: span(callee, callee + 2),
      ownerRef: "dr1",
    },
    record: {
      resolution: "resolved",
      declaredTarget: {
        kind: "internal",
        declarationRef: "dr1",
        revisionId: "r1",
      },
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "r1-call-proof",
    },
  };
  const oldUse = {
    kind: "reference",
    ref: "r1-use",
    anchor: {
      document,
      revisionId: "r1",
      contentHash: hash(source),
      kind: "reference",
      range: span(callee, callee + 2),
      ownerRef: "dr1",
    },
    record: {
      site: "use",
      roles: ["read", "call"],
      resolution: "resolved",
      declaredTarget: {
        kind: "internal",
        declarationRef: "dr1",
        revisionId: "r1",
      },
      candidates: [],
      provenanceId: "r1-reference-proof",
    },
  };
  const currentBinding = {
    ...oldBinding,
    ref: "r2-binding",
    anchor: {
      ...oldBinding.anchor,
      revisionId: "r2",
      contentHash: hash(r2),
      ownerRef: "dr2",
    },
    record: {
      ...oldBinding.record,
      declaredTarget: {
        kind: "internal",
        declarationRef: "dr2",
        revisionId: "r2",
      },
      provenanceId: "r2-call-proof",
    },
  };
  const raw = {
    formatVersion: 1,
    producerId: "semantic",
    facts: [
      ...(zero ? [olderFact] : [fact, oldBinding, oldUse]),
      ...(r2Binding ? [currentBinding] : []),
    ],
  };
  capture(
    "semantic-artifact",
    "semanticArtifact",
    "captures/semantic.json",
    JSON.stringify(raw),
  );
  const basis = {
    producerId: "semantic",
    producerVersion: "1",
    producerHash: producers[1].executableHash,
    artifactHash: captures.at(-1).hash,
    language: "javascript",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: sourceManifestHash([
      { document, contentHash: hash(source) },
    ]),
    toolchainHash: hash("toolchain"),
    configHash: hash("config"),
    dependencyHash: hash("dependency"),
    lookupDependencies: [],
  };
  const provenance = {
    kind: "provenance",
    ref: "r1-envelope",
    record: {
      id: "r1-proof",
      producerId: "semantic",
      document,
      revisionId: "r1",
      contentHash: hash(source),
      evidenceKind: "declarationBinding",
      basis,
      freshness: "fresh",
    },
  };
  const oldProofs = [
    ["r1-call-proof", "semanticReference"],
    ["r1-reference-proof", "semanticReference"],
  ].map(([id, evidenceKind]) => ({
    kind: "provenance",
    ref: `envelope-${id}`,
    record: {
      id,
      producerId: "semantic",
      document,
      revisionId: "r1",
      contentHash: hash(source),
      evidenceKind,
      basis,
      freshness: "fresh",
    },
  }));
  const olderProvenance = {
    kind: "provenance",
    ref: "r0-envelope",
    record: {
      ...provenance.record,
      id: "r0-proof",
      revisionId: "r0",
      basis: { ...basis, revisionId: "r0" },
    },
  };
  const coverageIntents = [],
    support = ["declarationName", "callee", "invocation", "reference"].map(
      (kind) => ({ kind, gaps: [], available: true, diagnostic: null }),
    );
  for (const id of revisionIds) {
    const facts = [];
    for (const producer of producers) {
      const semantic = producer.id === "semantic",
        status = semantic && id === "r2" ? state : "complete";
      const requestedRoles =
        semantic && !(readOnlyCurrent && id === "r2" && status === "complete")
          ? ["read", "call"]
          : ["read"];
      const observedRoles =
        (readOnlyCurrent || unsupportedCurrent) && semantic && id === "r2"
          ? ["read"]
          : status === "complete"
            ? requestedRoles
            : status === "partial"
              ? ["call"]
              : [];
      facts.push({
        kind: "coverage",
        ref: `coverage-${producer.id}-${id}`,
        record: {
          producerId: producer.id,
          sourceSetId: document.sourceSetId,
          language: document.language,
          documentPath: document.path,
          revisionId: id,
          requested: true,
          selected: status !== "omitted",
          state: status,
          supportedRoles: requestedRoles,
          observedRoles,
          diagnostic: status === "complete" ? null : "refresh unavailable",
        },
      });
      coverageIntents.push({
        producerId: producer.id,
        document,
        revisionId: id,
        requestedRoles,
        measurementSupport:
          (unsupportedCurrent || unsupportedInvocationCurrent) && id === "r2"
            ? support.map((row) =>
                row.kind === (unsupportedCurrent ? "callee" : "invocation")
                  ? {
                      kind: row.kind,
                      gaps: [], available: false,
                      diagnostic: `${row.kind} measurement unavailable`,
                    }
                  : row,
              )
            : support,
      });
    }
    if (id === "r0") facts.push(olderProvenance, olderFact);
    if (id === "r1" && !zero)
      facts.push(provenance, ...oldProofs, fact, oldBinding, oldUse);
    if (id === "r2" && r2Binding) {
      const currentBasis = {
        ...basis,
        revisionId: "r2",
        sourceManifestHash: sourceManifestHash([
          { document, contentHash: hash(r2) },
        ]),
      };
      facts.push(
        {
          kind: "provenance",
          ref: "r2-call-envelope",
          record: {
            id: "r2-call-proof",
            producerId: "semantic",
            document,
            revisionId: "r2",
            contentHash: hash(r2),
            evidenceKind: "semanticReference",
            basis: currentBasis,
            freshness: "fresh",
          },
        },
        currentBinding,
      );
    }
    put(`sources/${id}/go.js.annotations.json`, {
      formatVersion: 1,
      document,
      revisionId: id,
      scenarios: [],
      facts,
    });
  }
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions: [],
    callableValueNegatives: [],
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  const fixture = {
    formatVersion: 1,
    profile: "example",
    language: "javascript",
    sourceSets: [
      {
        id: "main",
        rootId: "root",
        languages: ["javascript"],
        dependencies: [],
      },
    ],
    producers,
    revisions,
    comparison: { sourceSetId: "main", revisionId: "r2", producers },
    coverageIntents,
    nativeArtifact: "captures/native.json",
    semanticArtifacts: ["captures/semantic.json"],
    annotationFiles: revisionIds.map(
      (id) => `sources/${id}/go.js.annotations.json`,
    ),
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  put("fixture.json", fixture);
  for (const [path, value] of files) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await writeFile(join(root, path), value);
  }
  const loaded = await loadFixture(root),
    records = normalizeFixture(loaded).records;
  records.producers.sort((a, b) =>
    Buffer.compare(canonicalBytes(a), canonicalBytes(b)),
  );
  records.revisions.sort((a, b) =>
    Buffer.compare(canonicalBytes(a), canonicalBytes(b)),
  );
  const checked = checkCoverage(loaded, records);
  const measured = checkMeasurement(loaded, records),
    joins = checkJoins(loaded, records, checked, measured);
  checkBindings(loaded, records, checked, measured, joins);
  return { loaded, records, checked };
}

test("captured failed and omitted refresh answers pass the complete graph checker", async (t) => {
  for (const options of [
    { state: "failed" },
    { state: "failed", changed: true },
    { state: "omitted" },
    { state: "omitted", changed: true },
    { state: "failed", zero: true },
  ]) {
    const { loaded, records, checked } = await admittedGraph(t, options);
    const rootId = syntaxId({
      sourceSet: "main",
      path: "src/go.js",
      language: "javascript",
      ancestors: [],
      declaration: {
        kind: "function",
        name: "go",
        signature: null,
        ordinal: 0,
      },
    });
    // Decision 0003: r2's call ID binds r2's go.js bytes, the empty-inventory
    // JavaScript extraction context and native producer `native`/`1`.
    const r2Text =
      "export function go() { go(); }\n" +
      (options.changed ? "// changed bytes\n" : "");
    const extractionContext = contentHash(
      Buffer.from(
        'trellis.extraction-context.v1\0{"components":[],"language":"javascript"}',
      ),
    );
const callId = occurrenceId({
      contentHash: contentHash(Buffer.from(r2Text)),
      extractionContext,
      nativeProducerId: "native",
      nativeProducerVersion: "1",
      ownerSyntaxId: rootId,
      kind: "call",
      ordinal: 0,
    });
    const declaration = records.declarations.find(
      (d) => d.revisionId === "r2" && d.syntaxId === rootId,
    );
    const call = records.calls.find(
      (c) => c.id === callId && c.revisionId === "r2",
    );
    assert.ok(declaration);
    assert.ok(call);
    assert.equal(
      records.callBindings.some((b) => b.provenanceId === "r1-call-proof"),
      !options.zero,
    );
    assert.equal(
      records.references.some((r) => r.provenanceId === "r1-reference-proof"),
      !options.zero,
    );
    const request = {
      sourceSetId: "main",
      revisionId: "r2",
      rootSyntaxId: rootId,
      semanticProducerId: "semantic",
      depth: 2,
      maxNodes: 150,
      maxCalls: 500,
    includeExternal: false,
    includeTests: false,
    };
    const proofIds = [
      `native:r2:${callId}`,
      `native:r2:${rootId}`,
      ...(options.zero ? [] : ["r1-proof"]),
    ];
    const provenance = proofIds
      .map((id) => records.provenance.find((p) => p.id === id))
      .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)));
    const coverage = records.coverage
      .filter(
        (row) =>
          row.revisionId === "r2" ||
          (row.revisionId === "r1" && row.producerId === "semantic"),
      )
      .sort(
        (a, b) =>
          Buffer.compare(
            Buffer.from(a.producerId),
            Buffer.from(b.producerId),
          ) ||
          Buffer.compare(Buffer.from(a.revisionId), Buffer.from(b.revisionId)),
      );
    const warnings = [
      ...(options.state === "failed"
        ? [
            {
              code: "coverageIncomplete",
              provenanceId: null,
              message: "refresh failed",
            },
          ]
        : []),
      ...(options.zero
        ? []
        : [
            {
              code: "staleEvidence",
              provenanceId: options.changed ? "r1-proof" : null,
              message: "historical declaration",
            },
          ]),
    ];
    const answer = {
      id: `${options.state}-${options.changed ? "changed" : "same"}-${options.zero ? "zero" : "fact"}`,
      attemptedRequest: request,
      answer: {
        ok: true,
        result: {
          request,
          resolvedRevisionId: "r2",
          nodes: [{ declaration, depth: 0 }],
          edges: [
            {
              call,
              callSiteCount: 1,
              from: rootId,
              to: null,
              binding: null,
              visit: "boundary",
              boundaryReason: "missingEvidence",
            },
          ],
          frontier: [],
          omitted: { filtered: 0, callLimit: 0, responseBytes: 0, depth: 0 },
          hints: [],
          coverage,
          provenance,
          partial: true,
          truncated: false,
          warnings,
        },
      },
    };
    assert.equal(
      provenance.some(
        (row) =>
          row.evidenceKind === "semanticReference" || row.id === "r0-proof",
      ),
      false,
    );
    if (options.zero)
      assert.ok(records.provenance.some((row) => row.id === "r0-proof"));
    assert.equal(
      provenance.find((row) => row.id === "r1-proof")?.freshness,
      options.zero ? undefined : options.changed ? "stale" : "possiblyStale",
    );
    assert.equal(
      checkAnswers(loaded, records, checked, { answers: [answer] }),
      true,
    );
  }
});

test("captured current binding: selected complete/partial self-call and failed leaked edge", async (t) => {
  for (const state of ["complete", "partial", "failed"]) {
    const { loaded, records, checked } = await admittedGraph(t, {
      state: state === "failed" ? "complete" : state,
      r2Binding: true,
    });
    if (state === "failed") {
      const tuple = records.coverage.find(
        (row) => row.producerId === "semantic" && row.revisionId === "r2",
      );
      tuple.state = "failed";
      tuple.selected = true;
      tuple.observedRoles = [];
      tuple.diagnostic = "refresh unavailable";
    }
    const declaration = records.declarations.find(
      (row) => row.revisionId === "r2",
    );
    const call = records.calls.find((row) => row.revisionId === "r2");
    const binding = records.callBindings.find(
      (row) => row.provenanceId === "r2-call-proof",
    );
    assert.ok(binding);
    assert.equal(binding.callId, call.id);
    assert.equal(binding.join.anchor.revisionId, "r2");
    assert.equal(
      loaded.semanticProofs.get("r2-call-proof").factRef,
      "r2-binding",
    );
    const usable = state !== "failed";
    const request = {
      sourceSetId: "main",
      revisionId: "r2",
      rootSyntaxId: declaration.syntaxId,
      semanticProducerId: "semantic",
      depth: 2,
      maxNodes: 150,
      maxCalls: 500,
    includeExternal: false,
    includeTests: false,
    };
    const proofIds = [
      declaration.provenanceId,
      call.provenanceId,
      ...(usable ? ["r2-call-proof"] : ["r1-proof"]),
    ];
    const provenance = proofIds
      .map((id) => records.provenance.find((row) => row.id === id))
      .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)));
    const coverage = records.coverage
      .filter(
        (row) =>
          row.revisionId === "r2" ||
          (!usable && row.revisionId === "r1" && row.producerId === "semantic"),
      )
      .sort(
        (a, b) =>
          Buffer.compare(
            Buffer.from(a.producerId),
            Buffer.from(b.producerId),
          ) ||
          Buffer.compare(Buffer.from(a.revisionId), Buffer.from(b.revisionId)),
      );
    const warnings = [
      ...(state === "complete"
        ? []
        : [
            {
              code: "coverageIncomplete",
              provenanceId: null,
              message: "selected coverage incomplete",
            },
          ]),
      ...(usable
        ? []
        : [
            {
              code: "staleEvidence",
              provenanceId: null,
              message: "historical declaration",
            },
          ]),
    ];
    const edge = {
      call,
      callSiteCount: 1,
      from: declaration.syntaxId,
      to: usable ? declaration.syntaxId : null,
      binding: usable ? binding : null,
      visit: usable ? "seen" : "boundary",
      boundaryReason: usable ? "none" : "missingEvidence",
    };
    const answer = {
      id: `captured-${state}`,
      attemptedRequest: request,
      answer: {
        ok: true,
        result: {
          request,
          resolvedRevisionId: "r2",
          nodes: [{ declaration, depth: 0 }],
          edges: [edge],
          frontier: [],
          omitted: { filtered: 0, callLimit: 0, responseBytes: 0, depth: 0 },
          hints: [],
          coverage,
          provenance,
          partial: !usable || state === "partial",
          truncated: false,
          warnings,
        },
      },
    };
    assert.equal(
      state === "failed"
        ? checkGraphAnswer(loaded, records, checked, answer)
        : checkAnswers(loaded, records, checked, { answers: [answer] }),
      true,
    );
    assert.equal(
      provenance.some(
        (row) => row.id === "r1-call-proof" || row.id === "r1-reference-proof",
      ),
      false,
    );
    if (!usable) {
      const row = registerControls([
        {
          id: "GRAPH.captured.failed-leaked-binding",
          baseline: () => answer,
          mutate: (x) => {
            x.answer.result.edges[0].binding = binding;
            return x;
          },
          check: (x) => checkGraphAnswer(loaded, records, checked, x),
          expectedAssertion: "GRAPH.TRAVERSAL",
          expectedCode: "invalidRecord",
          expectedField: "answers.captured-failed.result.edges",
        },
      ])[0];
      await t.test(row.id, () => runControl(row));
    }
  }
});

test("source-backed impossible r2 call facts reject atomically; absence remains a missingEvidence boundary", async (t) => {
  for (const state of ["failed", "omitted"]) {
    await t.test(
      `${state} tuple with physically present r2 binding`,
      async () => {
        await assert.rejects(
          () => admittedGraph(t, { state, r2Binding: true }),
          (error) => {
            assert.equal(error.assertion, "COVERAGE.FACT");
            assert.equal(error.code, "invalidRecord");
            assert.equal(error.field, "coverage");
            return true;
          },
        );
      },
    );
  }
  await t.test(
    "complete read-only tuple cannot carry a call binding",
    async () => {
      await assert.rejects(
        () =>
          admittedGraph(t, {
            state: "complete",
            r2Binding: true,
            readOnlyCurrent: true,
          }),
        (error) => {
          assert.equal(error.assertion, "COVERAGE.FACT");
          assert.equal(error.code, "invalidRecord");
          assert.equal(error.field, "coverage.observedRoles");
          return true;
        },
      );
    },
  );
  await t.test(
    "partial tuple not observing call cannot carry a call binding",
    async () => {
      await assert.rejects(
        () =>
          admittedGraph(t, {
            state: "partial",
            r2Binding: true,
            readOnlyCurrent: true,
          }),
        (error) => {
          assert.equal(error.assertion, "COVERAGE.FACT");
          assert.equal(error.code, "invalidRecord");
          assert.equal(error.field, "coverage.observedRoles");
          return true;
        },
      );
    },
  );
});

test("exact callee binding survives independently unsupported invocation measurement", async (t) => {
  const { loaded, records, checked } = await admittedGraph(t, {
    state: "complete",
    r2Binding: true,
    unsupportedInvocationCurrent: true,
  });
  const declaration = records.declarations.find(
    (row) => row.revisionId === "r2",
  );
  const call = records.calls.find((row) => row.revisionId === "r2");
  const binding = records.callBindings.find(
    (row) => row.provenanceId === "r2-call-proof",
  );
  assert.equal(binding.callId, call.id);
  assert.equal(binding.join.anchor.kind, "callee");
  assert.deepEqual(
    records.coverage.find(
      (row) => row.producerId === "semantic" && row.revisionId === "r2",
    ).observedRoles,
    ["read", "call"],
  );
  const request = {
    sourceSetId: "main",
    revisionId: "r2",
    rootSyntaxId: declaration.syntaxId,
    semanticProducerId: "semantic",
    depth: 2,
    maxNodes: 150,
    maxCalls: 500,
    includeExternal: false,
    includeTests: false,
  };
  const coverage = records.coverage
    .filter((row) => row.revisionId === "r2")
    .sort((a, b) =>
      Buffer.compare(Buffer.from(a.producerId), Buffer.from(b.producerId)),
    );
  const provenance = [
    declaration.provenanceId,
    call.provenanceId,
    binding.provenanceId,
  ]
    .map((id) => records.provenance.find((row) => row.id === id))
    .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)));
  const answer = {
    id: "callee-without-invocation-support",
    attemptedRequest: request,
    answer: {
      ok: true,
      result: {
        request,
        resolvedRevisionId: "r2",
        nodes: [{ declaration, depth: 0 }],
        edges: [
          {
            call,
            callSiteCount: 1,
            from: declaration.syntaxId,
            to: declaration.syntaxId,
            binding,
            visit: "seen",
            boundaryReason: "none",
          },
        ],
        frontier: [],
        omitted: { filtered: 0, callLimit: 0, responseBytes: 0, depth: 0 },
        hints: [],
        coverage,
        provenance,
        partial: false,
        truncated: false,
        warnings: [],
      },
    },
  };
  assert.equal(
    checkAnswers(loaded, records, checked, { answers: [answer] }),
    true,
  );
});

test("selected partial unsupported non-exact call stays diagnostic-only without observed call", async (t) => {
  const { loaded, records, checked } = await admittedGraph(t, {
    state: "partial",
    r2Binding: true,
    unsupportedCurrent: true,
  });
  const call = records.calls.find((row) => row.revisionId === "r2");
  const diagnostic = records.callBindings.find(
    (row) => row.provenanceId === "r2-call-proof",
  );
  assert.ok(call);
  assert.equal(diagnostic.callId, null);
  assert.equal(diagnostic.join.status, "unsupported");
  assert.deepEqual(
    records.coverage.find(
      (row) => row.producerId === "semantic" && row.revisionId === "r2",
    ).observedRoles,
    ["read"],
  );
  const declaration = records.declarations.find(
    (row) => row.revisionId === "r2",
  );
  const request = {
    sourceSetId: "main",
    revisionId: "r2",
    rootSyntaxId: declaration.syntaxId,
    semanticProducerId: "semantic",
    depth: 2,
    maxNodes: 150,
    maxCalls: 500,
    includeExternal: false,
    includeTests: false,
  };
  const coverage = records.coverage
    .filter((row) => row.revisionId === "r2")
    .sort((a, b) =>
      Buffer.compare(Buffer.from(a.producerId), Buffer.from(b.producerId)),
    );
  const provenance = [declaration.provenanceId, call.provenanceId]
    .map((id) => records.provenance.find((row) => row.id === id))
    .sort((a, b) => Buffer.compare(Buffer.from(a.id), Buffer.from(b.id)));
  const answer = {
    id: "unsupported-diagnostic",
    attemptedRequest: request,
    answer: {
      ok: true,
      result: {
        request,
        resolvedRevisionId: "r2",
        nodes: [{ declaration, depth: 0 }],
        edges: [
          {
            call,
            callSiteCount: 1,
            from: declaration.syntaxId,
            to: null,
            binding: null,
            visit: "boundary",
            boundaryReason: "missingEvidence",
          },
        ],
        frontier: [],
        omitted: { filtered: 0, callLimit: 0, responseBytes: 0, depth: 0 },
        hints: [],
        coverage,
        provenance,
        partial: true,
        truncated: false,
        warnings: [
          {
            code: "coverageIncomplete",
            provenanceId: null,
            message: "call role unavailable",
          },
        ],
      },
    },
  };
  assert.equal(
    checkAnswers(loaded, records, checked, { answers: [answer] }),
    true,
  );
  assert.equal(
    answer.answer.result.provenance.some((row) => row.id === "r2-call-proof"),
    false,
  );
});

test("captured reference roles must be observed by its selected producer/document tuple", async (t) => {
  const { loaded, records } = await admittedGraph(t, { state: "failed" });
  const tuple = records.coverage.find(
    (row) => row.producerId === "semantic" && row.revisionId === "r1",
  );
  const authored = loaded.annotations
    .flatMap((annotation) => annotation.facts)
    .find(
      (fact) =>
        fact.kind === "coverage" &&
        fact.record.producerId === "semantic" &&
        fact.record.revisionId === "r1",
    ).record;
  assert.deepEqual(tuple.observedRoles, ["read", "call"]);
  tuple.observedRoles = ["call"];
  tuple.state = "partial";
  tuple.diagnostic = "read role not observed";
  Object.assign(authored, {
    observedRoles: ["call"],
    state: "partial",
    diagnostic: "read role not observed",
  });
  assert.throws(
    () => checkCoverage(loaded, records),
    (error) => {
      assert.equal(error.assertion, "COVERAGE.FACT");
      assert.equal(error.field, "coverage.observedRoles");
      assert.match(error.message, /reference requires observed read role/);
      return true;
    },
  );
});

test("finite graph boundaries: cap with no work, defaults, syntax-only, resolution precedence", () => {
  {
    const s = specimen(),
      e = s.authored();
    e.attemptedRequest.maxCalls = e.answer.result.request.maxCalls = 0;
    if (e.answer.result.frontier.length) e.answer.result.hints = ["maxCalls", "rootSyntaxId"];
    assert.equal(verify(s, e), true);
    assert.equal(e.answer.result.frontier.length, 0);
    assert.equal(e.answer.result.truncated, false);
  }
  {
    const s = specimen(),
      e = s.authored({
        req: {
          sourceSetId: "main",
          revisionId: "r2",
          rootSyntaxId: sid(1),
          semanticProducerId: "P",
        },
      });
    e.answer.result.request = {
      ...e.answer.result.request,
      depth: 2,
      maxNodes: 150,
      maxCalls: 500,
    includeExternal: false,
    includeTests: false,
    };
    assert.equal(verify(s, e), true);
  }
  {
    const s = specimen();
    s.add(1, 2);
    const req = { ...s.request, semanticProducerId: null };
    const pair = { call: s.records.calls[0] };
    const e = s.authored({
      req,
      edges: [
        s.edge({ ...pair, binding: { declaredTarget: null } }, 1, {
          reason: "missingEvidence",
          binding: null,
        }),
      ],
      partial: true,
      warnings: [
        {
          code: "syntaxOnly",
          provenanceId: null,
          message: "native-only request",
        },
      ],
    });
    assert.equal(verify(s, e), true);
    assert.deepEqual(
      e.answer.result.coverage.map((row) => row.producerId),
      ["N"],
    );
  }
  {
    const s = specimen();
    const ambiguous = s.add(1, 2, {
      resolution: "ambiguous",
      staleTarget: null,
    });
    const unresolved = s.add(1, 2, {
      resolution: "unresolved",
      staleTarget: null,
    });
    const external = s.add(1, 2, { resolution: "external", staleTarget: null });
    const stale = s.add(1, 2, {
      resolution: "ambiguous",
      freshness: "stale",
      staleTarget: null,
    });
    const e = s.authored({
      edges: [
        s.edge(external, 1, { reason: "external" }),
        s.edge(ambiguous, 1, { reason: "ambiguous" }),
        s.edge(unresolved, 1, { reason: "unresolved" }),
        s.edge(stale, 1, { reason: "stale" }),
      ],
      proofIds: [
        ambiguous.binding.provenanceId,
        unresolved.binding.provenanceId,
        external.binding.provenanceId,
        stale.binding.provenanceId,
      ],
      partial: true,
      warnings: [
        {
          code: "staleEvidence",
          provenanceId: stale.binding.provenanceId,
          message: "stale",
        },
        {
          code: "bindingAmbiguous",
          provenanceId: ambiguous.binding.provenanceId,
          message: "ambiguous",
        },
        {
          code: "bindingAmbiguous",
          provenanceId: stale.binding.provenanceId,
          message: "ambiguous",
        },
      ],
    });
    e.attemptedRequest.includeExternal = e.answer.result.request.includeExternal = true;
    assert.equal(verify(s, e), true);
    assert.deepEqual(
      e.answer.result.edges.map((edge) => edge.boundaryReason),
      ["external", "ambiguous", "unresolved", "stale"],
    );
  }
});

test("historical tuple and fact selection exclude foreign producer, document, and unreturned declarations", () => {
  const s = specimen({ state: "failed" }),
    doc = { ...D, path: "src/foreign.js" };
  const selected = historical(s),
    unreturned = historical(s, { name: "B" }),
    foreignProducer = historical(s, {
      producer: "Q",
      id: "r1:foreign-producer",
    }),
    foreignDocument = historical(s, {
      document: doc,
      id: "r1:foreign-document",
    });
  const e = s.authored({
    extraCoverage: [s.records.coverage[2]],
    proofIds: [selected],
    partial: true,
    warnings: [
      { code: "coverageIncomplete", provenanceId: null, message: "r2 failed" },
      { code: "staleEvidence", provenanceId: null, message: "historical" },
    ],
  });
  assert.equal(verify(s, e), true);
  for (const id of [unreturned, foreignProducer, foreignDocument])
    assert.equal(
      e.answer.result.provenance.some((p) => p.id === id),
      false,
    );
});

test("graph assembly controls reject old occurrence, promoted and foreign facts, zero fallback and request/frontier drift", async (t) => {
  const graph = () => {
    const s = specimen({ state: "failed" }),
      pair = s.add(1, 2, { oldBinding: true }),
      proof = historical(s);
    const entry = s.authored({
      edges: [s.edge(pair, 1, { reason: "missingEvidence", binding: null })],
      extraCoverage: [s.records.coverage[2]],
      proofIds: [proof],
      partial: true,
      warnings: [
        { code: "coverageIncomplete", provenanceId: null, message: "failed" },
        {
          code: "staleEvidence",
          provenanceId: null,
          message: "possibly stale",
        },
      ],
    });
    return {
      loaded: s.loaded,
      records: s.records,
      entry,
      proof,
      oldBinding: pair.binding,
    };
  };
  const zero = () => {
    const s = specimen({ state: "failed" }),
      old = historical(s, { revision: "r0" }),
      entry = s.authored({
        extraCoverage: [s.records.coverage[2]],
        partial: true,
        warnings: [
          { code: "coverageIncomplete", provenanceId: null, message: "failed" },
        ],
      });
    return { loaded: s.loaded, records: s.records, entry, old };
  };
  const check = (x) =>
    checkGraphAnswer(x.loaded, x.records, { checkUse: () => true }, x.entry);
  const row = (id, baseline, mutate, assertion, field) => ({
    id,
    baseline,
    mutate,
    check,
    expectedAssertion: assertion,
    expectedCode: "invalidRecord",
    expectedField: field,
  });
  const rows = registerControls([
    row(
      "GRAPH.assembled.old-r1-binding",
      graph,
      (x) => {
        x.entry.answer.result.edges[0].binding = x.oldBinding;
        return x;
      },
      "GRAPH.TRAVERSAL",
      "answers.case.result.edges",
    ),
    row(
      "GRAPH.assembled.promoted-proof",
      graph,
      (x) => {
        x.records.provenance.find((p) => p.id === x.proof).freshness = "fresh";
        return x;
      },
      "GRAPH.HISTORY",
      "provenance",
    ),
    row(
      "GRAPH.assembled.relabelled-proof",
      graph,
      (x) => {
        x.records.provenance.find((p) => p.id === x.proof).revisionId = "r2";
        return x;
      },
      "GRAPH.HISTORY",
      "provenance",
    ),
    row(
      "GRAPH.assembled.unlinked-proof",
      graph,
      (x) => {
        x.loaded.semanticProofs.delete(x.proof);
        return x;
      },
      "GRAPH.HISTORY",
      "provenance",
    ),
    row(
      "GRAPH.assembled.unreturned-proof",
      graph,
      (x) => {
        const id = historical(
          { records: x.records, loaded: x.loaded },
          { name: "B" },
        );
        x.entry.answer.result.provenance.push(
          x.records.provenance.find((p) => p.id === id),
        );
        return x;
      },
      "GRAPH.PROVENANCE",
      "provenance",
    ),
    row(
      "GRAPH.assembled.foreign-producer",
      graph,
      (x) => {
        const id = historical(
          { records: x.records, loaded: x.loaded },
          { producer: "Q" },
        );
        x.entry.answer.result.provenance.push(
          x.records.provenance.find((p) => p.id === id),
        );
        return x;
      },
      "GRAPH.PROVENANCE",
      "provenance",
    ),
    row(
      "GRAPH.assembled.foreign-document",
      graph,
      (x) => {
        const id = historical(
          { records: x.records, loaded: x.loaded },
          {
            document: { ...D, path: "src/foreign.js" },
            id: "r1:foreign-document",
          },
        );
        x.entry.answer.result.provenance.push(
          x.records.provenance.find((p) => p.id === id),
        );
        return x;
      },
      "GRAPH.PROVENANCE",
      "provenance",
    ),
    row(
      "GRAPH.assembled.zero-fact-older-fallback",
      zero,
      (x) => {
        x.entry.answer.result.provenance.push(
          x.records.provenance.find((p) => p.id === x.old),
        );
        return x;
      },
      "GRAPH.PROVENANCE",
      "provenance",
    ),
    row(
      "GRAPH.assembled.effective-request",
      graph,
      (x) => {
        x.entry.answer.result.request = {
          ...x.entry.answer.result.request,
          depth: 1,
        };
        return x;
      },
      "GRAPH.TRAVERSAL",
      "answers.case.result.request",
    ),
    row(
      "GRAPH.assembled.frontier",
      graph,
      (x) => {
        x.entry.answer.result.frontier.push({
          reason: "depth",
          nodeId: sid(1),
          callId: null,
          targetId: null,
          nextOrdinal: 0,
          omittedCalls: 1,
        });
        return x;
      },
      "GRAPH.TRAVERSAL",
      "answers.case.result.frontier",
    ),
  ]);
  for (const control of rows)
    await t.test(control.id, () => runControl(control));
});


test("#379 test root caller role is exempt, while test callees and external calls stay filtered", () => {
  const s = specimen();
  s.records.declarations[0].role = "test";
  s.records.declarations[2].role = "test";
  const external = s.add(1, 2, { resolution: "external", staleTarget: null, freshness: "stale" });
  const internal = s.add(1, 2);
  s.add(1, 3);
  const entry = s.authored({ names: ["A", "B"], depths: [0, 1],
    edges: [s.edge(internal, 1)], proofIds: [internal.binding.provenanceId], partial: true });
  entry.answer.result.frontier = [s.front("filtered", 1, null, null, null, 2)];
  entry.answer.result.omitted.filtered = 2;
  entry.answer.result.hints = ["includeExternal", "includeTests"];
  assert.equal(verify(s, entry), true);
  assert.equal(entry.answer.result.truncated, false);
  assert.equal(entry.answer.result.provenance.some((p) => p.id === external.binding.provenanceId), false);

  const unknown = specimen();
  unknown.records.declarations[0].role = "test";
  const call = unknown.add(1, 2);
  unknown.records.callBindings = [];
  const visible = unknown.authored({ edges: [unknown.edge(call, 1,
    { reason: "missingEvidence", binding: null })], partial: true });
  assert.equal(verify(unknown, visible), true);
  assert.equal(visible.answer.result.omitted.filtered, 0);
});

test("#379 resolved test callee filtering does not follow ambiguous candidates and depth wins", () => {
  const s = specimen();
  s.records.declarations[1].role = "test";
  const exact = s.add(1, 2);
  const ambiguous = s.add(1, 2, { resolution: "ambiguous", staleTarget: null });
  ambiguous.binding.candidates = [exact.binding.declaredTarget];
  const entry = s.authored({ edges: [s.edge(ambiguous, 1, { reason: "ambiguous" })],
    proofIds: [ambiguous.binding.provenanceId], partial: true,
    warnings: [{ code: "bindingAmbiguous", provenanceId: ambiguous.binding.provenanceId, message: "ambiguous" }] });
  entry.answer.result.frontier = [s.front("filtered", 1, null, null, null, 1)];
  entry.answer.result.omitted.filtered = 1;
  entry.answer.result.hints = ["includeTests"];
  assert.equal(verify(s, entry), true);
  s.request.depth = 0;
  const depth = s.authored({ frontier: [s.front("depth", 1, null, null, 0, 2)], partial: true, truncated: true });
  assert.equal(verify(s, depth), true);
  assert.deepEqual(depth.answer.result.omitted, { filtered: 0, callLimit: 0, responseBytes: 0, depth: 2 });
});

test("#379 duplicate groups preserve earliest occurrence, exact proof and group call units", () => {
  const s = specimen();
  s.request.maxCalls = 1;
  const first = s.add(1, 2, { start: 1 });
  const second = s.add(1, 2, { start: 10 });
  second.binding.provenanceId = first.binding.provenanceId;
  const edge = { ...s.edge(first, 1), callSiteCount: 2 };
  const grouped = s.authored({ names: ["A", "B"], depths: [0, 1], edges: [edge], proofIds: [first.binding.provenanceId] });
  assert.equal(verify(s, grouped), true);
  assert.equal(grouped.answer.result.edges[0].call.id, first.call.id);
  assert.equal(grouped.answer.result.truncated, false);
  assert.throws(() => verify(s, { ...grouped, answer: { ok: true, result: {
    ...grouped.answer.result, edges: [{ ...edge, callSiteCount: 1 }] } } }), /result.edges/);
  second.binding.provenanceId = "P:2";
  const distinct = s.authored({ names: ["A", "B"], depths: [0, 1], edges: [s.edge(first, 1)],
    proofIds: [first.binding.provenanceId], frontier: [s.front("callLimit", 1, null, null, null, 1)],
    partial: true, truncated: true });
  assert.equal(verify(s, distinct), true);
});

test("#379 internal rank wins before external source order, and node refusal keeps rank", () => {
  const s = specimen();
  s.request.includeExternal = true;
  s.request.maxNodes = 2;
  const external = s.add(1, 2, { resolution: "external", staleTarget: null, start: 0 });
  const b = s.add(1, 2, { start: 10 });
  const c = s.add(1, 3, { start: 20 });
  const entry = s.authored({ names: ["A", "B"], depths: [0, 1],
    edges: [s.edge(b, 1), s.edge(c, 1, { reason: "nodeLimit" }), s.edge(external, 1, { reason: "external" })],
    proofIds: [b.binding.provenanceId, c.binding.provenanceId, external.binding.provenanceId],
    frontier: [s.front("nodeLimit", 1, c.call.id, sid(3))], partial: true, truncated: true });
  assert.equal(verify(s, entry), true);
  assert.equal(verify(s, structuredClone(entry)), true);
});

test("#379 invalid filter types and unknown fields are closed request failures", () => {
  for (const [override, field] of [[{ includeExternal: null }, "includeExternal"],
    [{ includeTests: "true" }, "includeTests"], [{ utilities: true }, "request"]]) {
    const s = specimen();
    const entry = { id: "invalid-filter", attemptedRequest: { ...s.request, ...override }, answer: {
      ok: false, error: { code: "invalidRequest", field, message: "invalid request" } } };
    assert.equal(verify(s, entry), true);
  }
});
