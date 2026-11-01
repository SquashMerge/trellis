import { canonicalBytes } from "./json.mjs";
import { graphProjection } from "./graph-projection.mjs";
import { selectGraphEvidence, checkGraphEvidence } from "./graph-evidence.mjs";
import { checkWarnings } from "./graph-warnings.mjs";
import { checkCoverage } from "./record-check/coverage.mjs";
import { checkMeasurement } from "./record-check/measurement.mjs";
import { checkJoins } from "./record-check/joins.mjs";
import { checkRelationships } from "./record-check/relationships.mjs";
import { checkBindings } from "./record-check/bindings.mjs";
import { checkAnchors } from "./record-check/anchors.mjs";

const bytes = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const equal = (a, b) => canonicalBytes(a).equals(canonicalBytes(b));
const documentKey = (d) => JSON.stringify([d.sourceSetId, d.language, d.path]);
const tuple = (producer, document, revision) =>
  JSON.stringify([producer, documentKey(document), revision]);
function fail(assertion, field, message) {
  const error = new Error(`${assertion} ${field}: ${message}`);
  Object.assign(error, { assertion, code: "invalidRecord", field });
  throw error;
}
function failure(code, field) {
  return {
    ok: false,
    error: {
      code,
      field,
      message: `${field} is not available for this graph request`,
    },
  };
}
const limits = [
  ["depth", 2, 0, 5],
  ["maxNodes", 150, 1, 150],
  ["maxCalls", 500, 0, 500],
];

// This source-derived projection does not trust the authored graph answer.
// The checked records, not a returned edge, choose bindings.
export function expectedGraph(loaded, records, checked, request) {
  const effective = { ...request };
  const fields = ["sourceSetId", "revisionId", "rootSyntaxId", "semanticProducerId", "depth", "maxNodes", "maxCalls", "includeExternal", "includeTests"];
  if (Object.keys(effective).some((field) => !fields.includes(field))) return failure("invalidRequest", "request");
  for (const field of ["includeExternal", "includeTests"]) {
    if (effective[field] === undefined) effective[field] = false;
    if (typeof effective[field] !== "boolean") return failure("invalidRequest", field);
  }
  for (const [name, defaultValue, min, max] of limits) {
    if (effective[name] === undefined) effective[name] = defaultValue;
    if (
      !Number.isSafeInteger(effective[name]) ||
      effective[name] < min ||
      effective[name] > max
    )
      return failure("invalidRequest", name);
  }
  if (
    typeof effective.sourceSetId !== "string" ||
    !effective.sourceSetId ||
    typeof effective.revisionId !== "string" ||
    !effective.revisionId ||
    typeof effective.rootSyntaxId !== "string" ||
    !/^sid:v1:[0-9a-f]{32}$/.test(effective.rootSyntaxId) ||
    !(
      effective.semanticProducerId === null ||
      (typeof effective.semanticProducerId === "string" &&
        effective.semanticProducerId.length)
    )
  )
    return failure("invalidRequest", "request");
  if (!records.sourceSets.some((s) => s.id === effective.sourceSetId))
    return failure("sourceSetDenied", "sourceSetId");
  if (
    !records.revisions.some(
      (r) =>
        r.id === effective.revisionId &&
        r.sourceSetId === effective.sourceSetId,
    )
  )
    return failure("revisionUnavailable", "revisionId");
  const declarations = records.declarations.filter(
    (d) =>
      d.revisionId === effective.revisionId &&
      d.document.sourceSetId === effective.sourceSetId,
  );
  const byId = new Map(declarations.map((d) => [d.syntaxId, d]));
  if (!byId.has(effective.rootSyntaxId))
    return failure("rootMissing", "rootSyntaxId");
  if (
    effective.semanticProducerId !== null &&
    !records.producers.some(
      (p) => p.id === effective.semanticProducerId && p.kind === "semantic",
    )
  )
    return failure("producerUnavailable", "semanticProducerId");
  const coverage = new Map(
    records.coverage.map((row) => [
      tuple(
        row.producerId,
        {
          sourceSetId: row.sourceSetId,
          language: row.language,
          path: row.documentPath,
        },
        row.revisionId,
      ),
      row,
    ]),
  );
  const projection = graphProjection(loaded, records, effective);
  const proofs = new Map(
    records.provenance.map((row) => [row.id, projection.proof(row)]),
  );
  const calls = new Map();
  for (const call of records.calls) {
    if (
      call.revisionId !== effective.revisionId ||
      call.document.sourceSetId !== effective.sourceSetId ||
      !byId.has(call.ownerSyntaxId)
    )
      continue;
    const owner = byId.get(call.ownerSyntaxId);
    if (documentKey(owner.document) !== documentKey(call.document)) continue;
    const local = calls.get(call.ownerSyntaxId) ?? [];
    local.push(call);
    calls.set(call.ownerSyntaxId, local);
  }
  for (const local of calls.values())
    local.sort(
      (a, b) =>
        bytes(a.document.path, b.document.path) ||
        a.range.start - b.range.start ||
        a.range.end - b.range.end ||
        bytes(a.id, b.id),
    );
  const bindings = new Map();
  if (effective.semanticProducerId !== null)
    for (const binding of records.callBindings) {
      if (
        binding.callId === null ||
        binding.join.status !== "exact" ||
        binding.join.candidateIds.length !== 1 ||
        binding.join.candidateIds[0] !== binding.callId
      )
        continue;
      const p = proofs.get(binding.provenanceId);
      if (
        p?.producerId !== effective.semanticProducerId ||
        p.revisionId !== effective.revisionId ||
        binding.join.anchor.revisionId !== effective.revisionId
      )
        continue;
      const local = bindings.get(binding.callId) ?? [];
      local.push(binding);
      bindings.set(binding.callId, local);
    }
  const nodes = [{ declaration: byId.get(effective.rootSyntaxId), depth: 0 }],
    edges = [], frontier = [], pending = [];
  const seen = new Set([effective.rootSyntaxId]);
  const hints = new Set();
  const omitted = { filtered: 0, callLimit: 0, responseBytes: 0, depth: 0 };
  const aggregate = (reason, nodeId, count) => {
    if (!count) return;
    omitted[reason] += count;
    const previous = frontier.find((f) => f.reason === reason && f.nodeId === nodeId);
    if (previous) previous.omittedCalls += count;
    else frontier.push({ reason, nodeId, callId: null, targetId: null,
      nextOrdinal: reason === "depth" ? 0 : null, omittedCalls: count });
  };
  const classify = (call, owner) => {
    const row = coverage.get(tuple(effective.semanticProducerId, call.document, effective.revisionId));
    const usable = row?.selected && ["complete", "partial"].includes(row.state);
    const selected = usable ? ((bindings.get(call.id) ?? [])
      .toSorted((a, b) => bytes(a.provenanceId, b.provenanceId))
      .find((b) => documentKey(b.join.anchor.document) === documentKey(call.document) &&
        b.join.anchor.contentHash === proofs.get(b.provenanceId)?.contentHash) ?? null) : null;
    const binding = selected === null ? null : projection.binding(selected);
    const proof = binding === null ? null : proofs.get(binding.provenanceId);
    let reason = "none";
    if (binding === null) reason = "missingEvidence";
    else if (proof.freshness !== "fresh" ||
      (binding.declaredTarget?.kind === "internal" && binding.staleTarget !== false)) reason = "stale";
    else if (binding.resolution === "ambiguous") reason = "ambiguous";
    else if (binding.resolution === "unresolved") reason = "unresolved";
    else if (binding.resolution === "external") reason = "external";
    else if (!["direct", "constructor"].includes(binding.dispatch)) reason = "dispatch";
    const target = binding?.declaredTarget;
    const exactInternal = binding?.resolution === "resolved" && target?.kind === "internal" &&
      byId.has(target.syntaxId) && documentKey(byId.get(target.syntaxId).document) === documentKey(target.document);
    if (reason === "none" && !exactInternal) reason = "stale";
    const external = binding?.resolution === "external";
    const test = (owner.syntaxId !== effective.rootSyntaxId && owner.role === "test") ||
      (exactInternal && byId.get(target.syntaxId).role === "test");
    return { call, binding, proof, target, reason, exactInternal, external, test,
      rank: reason === "none" ? 0 : external ? 1 : 2 };
  };
  const expand = ({ declaration, depth }) => {
    const local = calls.get(declaration.syntaxId) ?? [];
    if (depth === effective.depth) {
      aggregate("depth", declaration.syntaxId, local.length);
      return;
    }
    const groups = new Map();
    for (const call of local) {
      const fact = classify(call, declaration);
      if ((!effective.includeExternal && fact.external) || (!effective.includeTests && fact.test)) {
        aggregate("filtered", declaration.syntaxId, 1);
        if (!effective.includeExternal && fact.external) hints.add("includeExternal");
        if (!effective.includeTests && fact.test) hints.add("includeTests");
        continue;
      }
      // Occurrence-specific anchors are not group evidence; the selected proof,
      // resolution and disposition must still agree exactly.
      const { callId, join, ...outcome } = fact.binding ?? {};
      const key = fact.exactInternal ? canonicalBytes({ target: fact.target,
        outcome, join: { status: join.status, diagnostic: join.diagnostic ?? null },
        proof: fact.proof, reason: fact.reason, external: fact.external, test: fact.test }).toString() : call.id;
      const existing = groups.get(key);
      if (existing) existing.callSiteCount++;
      else groups.set(key, { ...fact, owner: declaration, depth, callSiteCount: 1 });
    }
    pending.push(...groups.values());
  };
  const compare = (a, b) => a.rank - b.rank || a.depth - b.depth ||
    bytes(a.owner.document.sourceSetId, b.owner.document.sourceSetId) ||
    bytes(a.owner.document.language, b.owner.document.language) ||
    bytes(a.owner.document.path, b.owner.document.path) || bytes(a.owner.syntaxId, b.owner.syntaxId) ||
    bytes(a.call.document.path, b.call.document.path) || a.call.range.start - b.call.range.start ||
    a.call.range.end - b.call.range.end || bytes(a.call.id, b.call.id);
  expand(nodes[0]);
  while (pending.length) {
    pending.sort(compare);
    const group = pending.shift();
    const { call, binding, target, owner, depth, callSiteCount } = group;
    if (edges.length === effective.maxCalls) {
      aggregate("callLimit", owner.syntaxId, callSiteCount);
      continue;
    }
    let reason = group.reason, to = null, visit = "boundary";
    if (reason === "dispatch" && group.exactInternal) {
      // The target's declaration, admitted but never expanded (#557).
      if (seen.has(target.syntaxId)) to = target.syntaxId;
      else if (nodes.length < effective.maxNodes) {
        to = target.syntaxId; seen.add(to);
        nodes.push({ declaration: byId.get(to), depth: depth + 1 });
      }
    }
    if (reason === "none") {
      if (seen.has(target.syntaxId)) { to = target.syntaxId; visit = "seen"; }
      else if (nodes.length === effective.maxNodes) {
        reason = "nodeLimit";
        frontier.push({ reason, nodeId: owner.syntaxId, callId: call.id,
          targetId: target.syntaxId, nextOrdinal: null, omittedCalls: 0 });
      } else {
        to = target.syntaxId; visit = "new"; seen.add(to);
        const node = { declaration: byId.get(to), depth: depth + 1 };
        nodes.push(node);
        expand(node);
      }
    }
    edges.push({ call, callSiteCount, from: owner.syntaxId, to, binding, visit, boundaryReason: reason });
  }
  const reasonOrder = ["depth", "nodeLimit", "callLimit", "filtered", "responseBytes"];
  const nodeOrder = new Map(nodes.map((n, i) => [n.declaration.syntaxId, i]));
  frontier.sort((a, b) => nodeOrder.get(a.nodeId) - nodeOrder.get(b.nodeId) ||
    reasonOrder.indexOf(a.reason) - reasonOrder.indexOf(b.reason) || bytes(a.callId ?? "", b.callId ?? "") ||
    bytes(a.targetId ?? "", b.targetId ?? ""));
  const truncated = frontier.some((f) => f.reason !== "filtered");
  if (omitted.depth && effective.depth < 5) hints.add("depth");
  if (omitted.callLimit && effective.maxCalls < 500) hints.add("maxCalls");
  if (frontier.some((f) => f.reason === "nodeLimit") && effective.maxNodes < 150) hints.add("maxNodes");
  if (truncated) hints.add("rootSyntaxId");
  const draft = {
    request: effective,
    resolvedRevisionId: effective.revisionId,
    nodes,
    edges,
    frontier,
    omitted,
    hints: [...hints].sort(bytes),
  };
  const evidence = selectGraphEvidence(loaded, records, checked, draft);
  return {
    ok: true,
    result: {
      ...draft,
      coverage: evidence.coverage,
      provenance: evidence.provenance,
      partial:
        truncated ||
        omitted.filtered > 0 ||
        evidence.selectedCoverageIncomplete ||
        edges.some((edge) => edge.visit === "boundary"),
      truncated,
    },
  };
}

export function checkAnswers(loaded, records, checked, materializedAnswers) {
  // A supplied coverage checker can be reused, but every downstream check still
  // runs in dependency order before the authored graph is considered.
  const coverage = checked?.checkUse ? checked : checkCoverage(loaded, records);
  const measured = checkMeasurement(loaded, records);
  const joins = checkJoins(loaded, records, coverage, measured);
  checkRelationships(loaded, records, coverage, measured, joins);
  checkBindings(loaded, records, coverage, measured, joins);
  checkAnchors(loaded, records, measured);
  const cases = materializedAnswers.answers;
  const ids = new Set();
  for (const entry of cases) {
    if (ids.has(entry.id))
      fail("GRAPH.CASES", "answers", "duplicate answer case");
    ids.add(entry.id);
    checkGraphAnswer(loaded, records, coverage, entry);
  }
  return true;
}

// Unit seam: callers provide an authored answer, not an expected traversal.
export function checkGraphAnswer(loaded, records, checked, entry) {
  const expected = expectedGraph(
    loaded,
    records,
    checked,
    entry.attemptedRequest,
  );
  if (entry.answer.ok !== expected.ok)
    fail("GRAPH.ANSWER", `answers.${entry.id}.ok`, "answer outcome differs");
  if (!expected.ok) {
    if (entry.answer.error.code !== expected.error.code)
      fail(
        "GRAPH.ERROR",
        `answers.${entry.id}.error.code`,
        "wrong failure code",
      );
    if (entry.answer.error.field !== expected.error.field)
      fail(
        "GRAPH.ERROR",
        `answers.${entry.id}.error.field`,
        "wrong failure field",
      );
    if (
      typeof entry.answer.error.message !== "string" ||
      !entry.answer.error.message.trim()
    )
      fail(
        "GRAPH.ERROR",
        `answers.${entry.id}.error.message`,
        "empty error explanation",
      );
    return true;
  }
  const actual = entry.answer.result,
    reference = expected.result;
  for (const field of [
    "request",
    "resolvedRevisionId",
    "nodes",
    "edges",
    "frontier",
    "omitted",
    "hints",
    "partial",
    "truncated",
  ])
    if (!equal(actual[field], reference[field]))
      fail(
        "GRAPH.TRAVERSAL",
        `answers.${entry.id}.result.${field}`,
        "authored graph differs from pinned source-derived traversal",
      );
  // The independent traversal, never the authored edges, selects proof rows.
  checkGraphEvidence(loaded, records, checked, actual, reference);
  checkWarnings(actual);
  return true;
}
