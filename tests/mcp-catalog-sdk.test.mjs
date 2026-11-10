// The MCP catalog must be a tools/list result that real MCP clients accept.
// Validate the exact catalog fixture (which the Rust catalog must equal) with
// the pinned MCP TypeScript SDK's ListToolsResultSchema (#158).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(new URL("./mcp/package.json", import.meta.url));
const { ListToolsResultSchema, ListPromptsResultSchema, GetPromptResultSchema } = require("@modelcontextprotocol/sdk/types.js");
const tools = JSON.parse(readFileSync(new URL("./fixtures/mcp/catalog.json", import.meta.url)));

test("the MCP catalog is a valid tools/list result for the MCP SDK", () => {
  for (const result of [{ tools }, { resultType: "complete", ttlMs: 0, cacheScope: "private", tools }]) {
    const parsed = ListToolsResultSchema.safeParse(result);
    assert.ok(parsed.success, JSON.stringify(parsed.error?.issues));
  }
  for (const tool of tools) {
    assert.equal(tool.inputSchema.type, "object", `${tool.name} inputSchema`);
    assert.equal(tool.outputSchema.type, "object", `${tool.name} outputSchema`);
  }
});


test("find and declaration schemas require closed declaration roles", () => {
  for (const [toolName, definitionName] of [
    ["trellis_find_symbols", "Symbol"],
    ["trellis_search_symbols", "SearchHit"],
    ["trellis_declaration", "Declaration"],
  ]) {
    const definition = tools.find(tool => tool.name === toolName).outputSchema.$defs[definitionName];
    assert.deepEqual(definition.properties.role.enum, ["test", "production"]);
    assert.ok(definition.required.includes("role"));
    assert.equal(definition.additionalProperties, false);
  }
});


test("architecture prompt messages and catalog use SDK prompt shapes", () => {
  const prompts = [{name: "trellis_propose_rules", description: "Read-only layer proposal", arguments: []}];
  for (const result of [{prompts}, {resultType: "complete", ttlMs: 0, cacheScope: "private", prompts}]) {
    assert.ok(ListPromptsResultSchema.safeParse(result).success);
  }
  const messages = [{role: "user", content: {type: "text", text: "Propose a rules diff for human review"}}];
  for (const result of [{messages}, {resultType: "complete", messages}]) {
    assert.ok(GetPromptResultSchema.safeParse(result).success);
  }
  assert.equal(tools.length, 13);
  assert.ok(!tools.some(tool => tool.name === "trellis_propose_rules"));
});

// Draft 2020-12 is the advertised wire schema, rather than an SDK-only shape check.
const Ajv2020 = require("ajv/dist/2020.js").default;
const ajv = new Ajv2020({ strict: false });
const search = tools.find(tool => tool.name === "trellis_search_symbols");

test("the thirteen-tool order and unchanged schemas are explicit", () => {
  assert.deepEqual(tools.map(tool => tool.name), [
    "trellis_workspace_describe", "trellis_find_symbols", "trellis_search_symbols",
    "trellis_declaration", "trellis_outgoing_calls", "trellis_incoming_calls",
    "trellis_read_source", "trellis_call_graph", "trellis_references", "trellis_rules_check",
    "trellis_rules_explain", "trellis_show", "trellis_endpoints",
  ]);
  const describe = tools[0].outputSchema.oneOf[0].properties.data.properties;
  assert.equal(describe.toolVersion.const, 24);
  assert.equal(search.inputSchema.additionalProperties, false);
  assert.equal(search.inputSchema.properties.limit.default, 10);
  assert.equal(search.inputSchema.properties.limit.maximum, 50);
  const findSymbol = tools[1].outputSchema.$defs.Symbol;
  const { rank, matchedBy, ...symbolFields } = search.outputSchema.$defs.SearchHit.properties;
  assert.deepEqual(symbolFields, findSymbol.properties);
  assert.deepEqual(search.outputSchema.$defs.SearchHit.required,
    [...findSymbol.required, "rank", "matchedBy"]);
});

test("describe configuration is optional, closed, and separate from query readiness", () => {
  const schema = tools[0].outputSchema;
  const validate = ajv.compile({ $schema: schema.$schema, $defs: schema.$defs,
    $ref: "#/$defs/SearchAvailability" });
  const availability = { backend: "cpu", unavailableReason: "model_not_configured" };
  const configuration = {
    configFile: "/home/test/.config/trellis/config.toml", modelsDir: "/home/test/models",
    modelName: "voyageai/voyage-4-nano", modelRevision: "reviewed-revision",
    modelPath: "/home/test/models/voyageai/voyage-4-nano/reviewed-revision", backend: "auto",
    sources: { modelsDir: "default", model: "config_file", backend: "environment" },
  };
  assert.ok(validate(availability), JSON.stringify(validate.errors));
  assert.ok(validate({ ...availability, configuration }), JSON.stringify(validate.errors));
  assert.ok(validate({ ...availability, configuration: { ...configuration, modelPath: null } }));
  for (const field of Object.keys(configuration)) {
    const incomplete = { ...configuration };
    delete incomplete[field];
    assert.equal(validate({ ...availability, configuration: incomplete }), false, field);
  }
  for (const invalid of [
    null,
    { ...configuration, extra: true },
    { ...configuration, backend: "gpu" },
    { ...configuration, modelPath: 7 },
    { ...configuration, sources: { ...configuration.sources, model: "other" } },
    { ...configuration, sources: { ...configuration.sources, extra: "default" } },
    { ...configuration, sources: { model: "default", backend: "default" } },
  ]) {
    assert.equal(validate({ ...availability, configuration: invalid }), false);
  }
  const validateReadiness = ajv.compile({ $schema: search.outputSchema.$schema,
    $defs: search.outputSchema.$defs, $ref: "#/$defs/SearchIndexStatus" });
  const readiness = { state: "unavailable", modelId: null, modelName: null, backend: "cpu",
    declarations: 1, embedded: 0, lexical: "complete", unavailableReason: "model_not_configured" };
  assert.ok(validateReadiness(readiness), JSON.stringify(validateReadiness.errors));
  assert.equal(validateReadiness({ ...readiness, configuration }), false);
});

test("native coverage labels do not claim the selected semantic tier", () => {
  for (const tool of tools) {
    const schema = tool.outputSchema;
    if (!schema.$defs?.LanguageStatus) continue;
    const validate = ajv.compile({ $schema: schema.$schema, $defs: schema.$defs,
      $ref: "#/$defs/LanguageStatus" });
    const coverage = { selected: 2, complete: 1, partial: 1, failed: 0, unselected: 0 };
    const semantic = { available: true, producerId: "rust-analyzer", freshness: "fresh",
      coverage, note: null, reimport: null, documents: [] };
    if (tool.name === "trellis_workspace_describe") semantic.partialReasons = null;
    const status = { language: "rust", nativeCoverageTier: "syntax", coverage, semantic };
    assert.ok(validate(status), `${tool.name}: ${JSON.stringify(validate.errors)}`);
    assert.equal(validate({ ...status, nativeCoverageTier: "semantic" }), false, tool.name);
    const { nativeCoverageTier, ...legacy } = status;
    assert.equal(validate({ ...legacy, evidenceTier: "syntax" }), false, tool.name);
    assert.equal(validate({ ...status, evidenceTier: "syntax" }), false, tool.name);
    const reimport = { state: "needsReimport", reason: "producer profile pin changed" };
    assert.ok(validate({ ...status, semantic: { ...semantic, reimport } }), tool.name);
    const { reimport: _, ...missing } = semantic;
    assert.equal(validate({ ...status, semantic: missing }), false, tool.name);
    for (const invalid of [{ state: "fresh", reason: "changed" }, { state: "needsReimport" },
      { ...reimport, extra: true }, { ...reimport, reason: "" }]) {
      assert.equal(validate({ ...status, semantic: { ...semantic, reimport: invalid } }), false, tool.name);
    }
  }
  // The field rename must not rewrite item-level evidence semantics.
  assert.deepEqual(tools[1].outputSchema.$defs.Symbol.properties.evidenceTier,
    { const: "syntax" });
  assert.deepEqual(tools[4].outputSchema.$defs.CallItem.properties.evidenceTier.enum,
    ["syntax", "semantic"]);
});

test("search accepts only its closed public input", () => {
  const validate = ajv.compile(search.inputSchema);
  const valid = { schemaVersion: 1, query: "  measured query  " };
  assert.ok(validate(valid), JSON.stringify(validate.errors));
  assert.ok(validate({ ...valid, query: "x".repeat(1024), limit: 50 }));
  for (const query of ["", "x".repeat(1025), null, 1]) {
    assert.equal(validate({ ...valid, query }), false);
  }
  for (const field of ["filter", "filters", "backend", "score", "scores", "debug", "cursor", "index", "view"]) {
    assert.equal(validate({ ...valid, [field]: true }), false, field);
  }
  for (const limit of [0, 51, -1, 1.5, null, "10"]) {
    assert.equal(validate({ ...valid, limit }), false);
  }
});

test("search status and rank membership definitions are closed", () => {
  const defs = search.outputSchema.$defs;
  const definition = name => ajv.compile({ $schema: search.outputSchema.$schema,
    $defs: defs, $ref: `#/$defs/${name}` });
  const validateStatus = definition("SearchIndexStatus");
  const status = { state: "unavailable", modelId: null, modelName: null, backend: null,
    declarations: 3, embedded: 0, lexical: "complete", unavailableReason: null };
  for (const backend of [null, "cpu", "metal"]) {
    assert.ok(validateStatus({ ...status, backend }), JSON.stringify(validateStatus.errors));
  }
  assert.ok(validateStatus({ ...status, unavailableReason: "vector_budget_exceeded" }));
  assert.equal(validateStatus({ ...status, unavailableReason: "other" }), false);
  assert.equal(validateStatus({ ...status, backend: "auto" }), false);
  assert.equal(validateStatus({ ...status, score: 1 }), false);
  for (const field of Object.keys(status)) {
    const missing = { ...status };
    delete missing[field];
    assert.equal(validateStatus(missing), false, field);
  }
  const validateArms = ajv.compile(defs.SearchHit.properties.matchedBy);
  for (const arms of [["lexical"], ["vector"], ["lexical", "vector"]]) {
    assert.ok(validateArms(arms));
  }
  for (const arms of [[], ["vector", "lexical"], ["lexical", "lexical"], ["other"]]) {
    assert.equal(validateArms(arms), false);
  }
  const validateRank = ajv.compile(defs.SearchHit.properties.rank);
  assert.ok(validateRank(1));
  for (const rank of [0, -1, 1.5, null]) assert.equal(validateRank(rank), false);
});

test("call graph accepts only its closed, unclamped public input", () => {
  const graph = tools.find(tool => tool.name === "trellis_call_graph");
  const validate = ajv.compile(graph.inputSchema);
  const valid = { schemaVersion: 1, symbolId: `sid:v1:${"a".repeat(32)}` };
  assert.ok(validate(valid), JSON.stringify(validate.errors));
  assert.ok(validate({ ...valid, depth: 0, maxNodes: 1, maxCalls: 0 }));
  assert.ok(validate({ ...valid, depth: 5, maxNodes: 150, maxCalls: 500, semanticProducerId: null }));
  assert.ok(validate({ ...valid, semanticProducerId: "scip-typescript" }));
  for (const depth of [6, -1, 1.5, null, "2"]) assert.equal(validate({ ...valid, depth }), false, `depth ${depth}`);
  for (const maxNodes of [0, 151, 1.5, null, "2"]) assert.equal(validate({ ...valid, maxNodes }), false, `maxNodes ${maxNodes}`);
  for (const maxCalls of [-1, 501, 1.5, null, "2"]) assert.equal(validate({ ...valid, maxCalls }), false, `maxCalls ${maxCalls}`);
  for (const semanticProducerId of ["", 1, true]) assert.equal(validate({ ...valid, semanticProducerId }), false);
  for (const field of ["limit", "view", "revisionId", "sourceSetId", "cursor", "unknown"]) {
    assert.equal(validate({ ...valid, [field]: 1 }), false, field);
  }
  assert.equal(validate({ schemaVersion: 1 }), false);
  assert.equal(validate({ ...valid, symbolId: "sid:v1:ABC" }), false);
  assert.equal(graph.inputSchema.properties.depth.default, 2);
  // #665: defaults sized to the response; compact:true has its own larger defaults.
  assert.equal(graph.inputSchema.properties.maxNodes.default, 8);
  assert.equal(graph.inputSchema.properties.maxCalls.default, 10);
  assert.equal(graph.inputSchema.properties.compact.default, false);
  assert.ok(validate({ ...valid, compact: true }));
  for (const compact of [null, 1, "true"]) assert.equal(validate({ ...valid, compact }), false);
});

test("outgoing calls accept a closed includeExternal; incoming calls do not", () => {
  const valid = { schemaVersion: 1, symbolId: `sid:v1:${"a".repeat(32)}` };
  const outgoing = ajv.compile(tools.find(tool => tool.name === "trellis_outgoing_calls").inputSchema);
  const incoming = ajv.compile(tools.find(tool => tool.name === "trellis_incoming_calls").inputSchema);
  for (const includeExternal of [true, false]) {
    assert.ok(outgoing({ ...valid, compact: true, includeExternal }));
  }
  for (const includeExternal of [null, 1, "true"]) assert.equal(outgoing({ ...valid, includeExternal }), false);
  assert.equal(incoming({ ...valid, includeExternal: true }), false);
});

// #818: the advertised input schema admits exactly the per-view argument shapes the server does.
test("trellis_show input admits one target per view, and its outcomes are closed", () => {
  const show = tools.find(tool => tool.name === "trellis_show");
  assert.deepEqual(show.annotations, { destructiveHint: false, idempotentHint: false, openWorldHint: false, readOnlyHint: true });
  const input = ajv.compile(show.inputSchema);
  const sid = `sid:v1:${"a".repeat(32)}`;
  for (const ok of [
    { schemaVersion: 1, view: "sequence", symbolId: sid },
    { schemaVersion: 1, view: "class", lookupKey: "src/a.rs::A" },
    { schemaVersion: 1, view: "source", path: "src/a.rs", line: 1, workspace: "/repo" },
  ]) assert.ok(input(ok), JSON.stringify([ok, input.errors]));
  for (const bad of [
    { schemaVersion: 1, view: "sequence" },
    { schemaVersion: 1, view: "sequence", symbolId: sid, lookupKey: "k" },
    { schemaVersion: 1, view: "class", symbolId: sid, line: 2 },
    { schemaVersion: 1, view: "source", path: "src/a.rs" },
    { schemaVersion: 1, view: "source", path: "src/a.rs", line: 0 },
    { schemaVersion: 1, view: "source", path: "src/a.rs", line: 1, symbolId: sid },
    { schemaVersion: 1, view: "architecture", symbolId: sid },
    { schemaVersion: 1, view: "sequence", symbolId: sid, other: true },
  ]) assert.ok(!input(bad), JSON.stringify(bad));
  const output = ajv.compile(show.outputSchema);
  const envelope = (data) => ({ schemaVersion: 1, requestId: 1, workspace: "/repo", catchingUp: false,
    evidenceBasis: { indexGeneration: "12345678-1234-4123-8123-123456789abc", indexRevision: 1 },
    data: { view: "sequence", message: "m", ...data }, warnings: [], partial: false, truncated: false, truncationReason: null });
  assert.ok(output(envelope({ outcome: "shown", generation: 7 })), JSON.stringify(output.errors));
  for (const outcome of ["no_viewer", "not_found", "stale", "invalid"]) assert.ok(output(envelope({ outcome, generation: null })), outcome);
  assert.ok(!output(envelope({ outcome: "shown", generation: null })), "shown carries its generation");
  assert.ok(!output(envelope({ outcome: "stale", generation: 7 })), "only shown has a generation");
  assert.ok(!output(envelope({ outcome: "partial", generation: null })));
});

// #798: the building error of each architecture tool takes exactly the ratified shape,
// with the admitted basis and a null content hash.
test("architecture building failures require a basis and a null content hash", () => {
  const basis = { indexGeneration: "123e4567-e89b-42d3-a456-426614174000", indexRevision: 7 };
  const valid = {
    schemaVersion: 1, requestId: 1, workspace: "/w", catchingUp: false,
    error: {
      code: "building", message: "Architecture is being computed for this revision; retry shortly",
      retryable: true, currentBasis: basis, currentContentHash: null,
      progress: { phase: "measuring", completed: 1, total: 3 }, retryAfterMs: 250,
    },
  };
  const variant = (change) => {
    const copy = structuredClone(valid);
    change(copy.error);
    return copy;
  };
  for (const name of ["trellis_references", "trellis_rules_check", "trellis_rules_explain"]) {
    const validate = ajv.compile(tools.find((tool) => tool.name === name).outputSchema);
    assert.ok(validate(valid), `${name}: ${JSON.stringify(validate.errors)}`);
    assert.ok(validate(variant((error) => { error.progress = null; })), name);
    for (const [label, change] of [
      ["null basis", (error) => { error.currentBasis = null; }],
      ["content hash", (error) => { error.currentContentHash = "a".repeat(64); }],
      ["null basis and content hash", (error) => { error.currentBasis = null; error.currentContentHash = "a".repeat(64); }],
      ["not retryable", (error) => { error.retryable = false; }],
      ["no retryAfterMs", (error) => { delete error.retryAfterMs; }],
    ]) {
      assert.equal(validate(variant(change)), false, `${name} accepted ${label}`);
    }
  }
});

// #780: describe's opt-in coverage document request and page are closed.
test("describe coverage documents are an opt-in closed request and page", () => {
  const describe = tools[0];
  const input = ajv.compile(describe.inputSchema);
  for (const coverageDocuments of [{}, { language: "rust" }, { language: "typescript", limit: 50, offset: 7 }]) {
    assert.ok(input({ schemaVersion: 1, coverageDocuments }), JSON.stringify(input.errors));
  }
  for (const coverageDocuments of [null, true, { language: "go" }, { limit: 0 }, { limit: 51 },
    { offset: -1 }, { state: "failed" }]) {
    assert.equal(input({ schemaVersion: 1, coverageDocuments }), false, JSON.stringify(coverageDocuments));
  }
  const data = describe.outputSchema.oneOf[0].properties.data;
  assert.ok(!data.required.includes("coverageDocuments"));
  const page = ajv.compile({ $schema: describe.outputSchema.$schema, $defs: describe.outputSchema.$defs,
    ...data.properties.coverageDocuments });
  const item = { language: "rust", path: "src/b.rs", state: "failed", reasons: ["notInProducerOutput"] };
  const valid = { items: [item, { ...item, state: "partial", reasons: ["nativeGaps", "capped"] }],
    page: { offset: 0, nextOffset: null, total: 2 } };
  assert.ok(page(valid), JSON.stringify(page.errors));
  for (const invalid of [
    { ...valid, items: [{ ...item, state: "complete" }] },
    { ...valid, items: [{ ...item, reasons: [] }] },
    { ...valid, items: [{ ...item, reasons: ["excluded_failed_by_producer"] }] },
    { ...valid, items: [{ ...item, diagnostic: "x" }] },
    { items: [] },
  ]) assert.equal(page(invalid), false, JSON.stringify(invalid));
});

// #775: references groupBy is closed, depth is bounded, and group rows are closed.
test("references groupBy input and group rows are closed", () => {
  const references = tools.find(tool => tool.name === "trellis_references");
  const input = ajv.compile(references.inputSchema);
  for (const ok of [{ groupBy: "file" }, { groupBy: "directory", depth: 1 }, { groupBy: "package", limit: 50 }]) {
    assert.ok(input({ schemaVersion: 1, ...ok }), JSON.stringify(input.errors));
  }
  for (const bad of [{ groupBy: "module" }, { groupBy: null }, { groupBy: "directory", depth: 0 },
    { groupBy: "directory", depth: 33 }, { groupBy: "directory", depth: "2" }]) {
    assert.equal(input({ schemaVersion: 1, ...bad }), false, JSON.stringify(bad));
  }
  const data = references.outputSchema.oneOf[0].properties.data;
  assert.ok(data.required.includes("groups") && data.required.includes("groupBy"));
  const rows = ajv.compile({ $schema: references.outputSchema.$schema, $defs: references.outputSchema.$defs,
    ...data.properties.groups });
  const serde = { kind: "externalPackage", sourceSetId: "s", language: "rust", package: "serde", standardLibrary: false };
  const row = { source: "src/app", target: "src/lib", sites: 3, production: 2, test: 1 };
  assert.ok(rows([row, { ...row, target: serde }, { ...row, target: null }]), JSON.stringify(rows.errors));
  for (const invalid of [{ ...row, target: { ...serde, extra: 1 } }, { ...row, kindCounts: {} },
    (({ test, ...rest }) => rest)(row)]) {
    assert.equal(rows([invalid]), false, JSON.stringify(invalid));
  }
});

// #836: the compacted failure schemas accept exactly the failures the four separate
// no-workspace shapes and the resolved, located and building shapes accepted before. The
// oracle below restates those shapes from the contract, independently of the catalog.
test("compacted failure schemas keep every failure shape exact", () => {
  const { AjvJsonSchemaValidator } = require("@modelcontextprotocol/sdk/validation/ajv");
  const sdk = new AjvJsonSchemaValidator();
  const basis = { indexGeneration: "123e4567-e89b-42d3-a456-426614174000", indexRevision: 7 };
  const resolved = ["invalid_request", "range_too_large", "unsupported_encoding", "revision_conflict",
    "index_not_ready", "not_found", "too_many_requests", "deadline_exceeded", "rebuild_in_progress",
    "root_changed", "store_unavailable", "checkout_capacity", "daemon_unavailable", "outcome_unknown"];
  const reasons = ["not_absolute", "not_checkout", "identity_changed", "unavailable"];
  const architecture = ["trellis_references", "trellis_rules_check", "trellis_rules_explain",
    "trellis_endpoints"];
  // #851: an `app` filter of these fails as the rules check does.
  const appFiltered = ["trellis_find_symbols", "trellis_search_symbols", "trellis_call_graph"];
  const keys = (object, allowed) => Object.keys(object).every((key) => allowed.includes(key));
  const common = ["code", "message", "retryable", "currentBasis", "currentContentHash"];
  const expected = (name, failure) => {
    const { error } = failure;
    if (!common.every((key) => key in error) || typeof error.message !== "string") return false;
    if ("workspace" in failure) {
      if (!keys(failure, ["schemaVersion", "requestId", "error", "workspace", "catchingUp"])) return false;
      if (typeof error.retryable !== "boolean" || error.currentContentHash !== null && typeof error.currentContentHash !== "string") return false;
      if (error.code === "building")
        return architecture.includes(name) && keys(error, [...common, "progress", "retryAfterMs"])
          && "progress" in error && "retryAfterMs" in error && error.retryable === true
          && error.currentBasis !== null && error.currentContentHash === null;
      const codes = architecture.includes(name) ? [...resolved, "building", "invalid_rules", "bounded_work"]
        : appFiltered.includes(name) ? [...resolved, "invalid_rules", "bounded_work"] : resolved;
      return codes.includes(error.code) && error.code !== "building" && keys(error, common);
    }
    if (!keys(failure, ["schemaVersion", "requestId", "error"])) return false;
    if (error.currentBasis !== null || error.currentContentHash !== null) return false;
    const attempted = "attemptedWorkspace" in error;
    if (error.code === "workspace_selection_failed")
      return attempted && reasons.includes(error.reason) && keys(error, [...common, "attemptedWorkspace", "reason"])
        && error.retryable === (error.reason === "unavailable");
    if ("reason" in error) return false;
    if (error.code === "invalid_request" && attempted)
      return error.retryable === false && keys(error, [...common, "attemptedWorkspace"]);
    if (["too_many_requests", "deadline_exceeded"].includes(error.code))
      return attempted && error.retryable === true && keys(error, [...common, "attemptedWorkspace"]);
    return ["invalid_request", "range_too_large"].includes(error.code) && !attempted
      && error.retryable === false && keys(error, common);
  };
  const failures = [];
  for (const code of [...resolved, "workspace_selection_failed", "building", "invalid_rules", "bounded_work", "bogus"])
    for (const retryable of [true, false])
      for (const reason of [undefined, "unavailable", "not_checkout", "different_repository"])
        for (const attemptedWorkspace of [undefined, { kind: "invalid_type", jsonType: "array" }])
          for (const currentBasis of [null, basis])
            for (const extra of [{}, { progress: null, retryAfterMs: 250 }, { junk: 1 }])
              for (const workspace of [false, true]) {
                const error = { code, message: "m", retryable, currentBasis, currentContentHash: null, ...extra };
                if (reason !== undefined) error.reason = reason;
                if (attemptedWorkspace !== undefined) error.attemptedWorkspace = attemptedWorkspace;
                const failure = { schemaVersion: 1, requestId: "r", error };
                if (workspace) Object.assign(failure, { workspace: "/w", catchingUp: false });
                failures.push(failure);
              }
  let accepted = 0;
  for (const tool of tools) {
    const validate = sdk.getValidator(tool.outputSchema);
    for (const failure of failures) {
      const want = expected(tool.name, failure);
      accepted += want;
      assert.equal(validate(failure).valid, want, `${tool.name} ${JSON.stringify(failure)}`);
    }
  }
  assert.ok(accepted > 500, `${accepted} accepted failures`);
});

// #851: the `app` filter is one bounded string on exactly the tools that take it.
test("the app filter is a closed string on references, check, find, search, the call graph and endpoints", () => {
  const sid = `sid:v1:${"a".repeat(32)}`;
  const base = {
    trellis_references: {}, trellis_rules_check: {}, trellis_endpoints: {},
    trellis_find_symbols: { query: "a" }, trellis_search_symbols: { query: "a" },
    trellis_call_graph: { symbolId: sid },
  };
  for (const tool of tools) {
    const input = ajv.compile(tool.inputSchema);
    const args = base[tool.name];
    if (!args) {
      assert.ok(!("app" in tool.inputSchema.properties), tool.name);
      continue;
    }
    for (const app of ["web", "web/package.json", "a".repeat(4096)])
      assert.ok(input({ schemaVersion: 1, ...args, app }), `${tool.name} ${JSON.stringify(input.errors)}`);
    for (const app of ["", "a".repeat(4097), 7, null])
      assert.equal(input({ schemaVersion: 1, ...args, app }), false, `${tool.name} ${JSON.stringify(app)}`);
  }
});

// #851: describe's apps are a closed inventory, required on every answer.
test("describe apps are a closed, required inventory", () => {
  const describe = tools[0];
  const data = describe.outputSchema.oneOf[0].properties.data;
  assert.ok(data.required.includes("apps"));
  const apps = ajv.compile({ $defs: describe.outputSchema.$defs, ...data.properties.apps });
  const app = { name: "web", source: "manifest", kind: "npm", manifest: "web/package.json", roots: ["web"],
    entries: [], languages: ["javascript"], fileCount: 2 };
  const authored = { ...app, name: "console", source: "override", kind: null, manifest: null, roots: ["admin"] };
  for (const ok of [
    { state: "ready", items: [authored, app], noApp: { fileCount: 0 } },
    { state: "building", items: [], noApp: null },
    { state: "invalidRules", items: [], noApp: null },
    { state: "unavailable", items: [], noApp: null },
  ]) assert.ok(apps(ok), JSON.stringify(apps.errors));
  for (const bad of [
    { state: "partial", items: [], noApp: null },
    { state: "ready", items: [{ ...app, kind: "maven" }], noApp: null },
    { state: "ready", items: [{ ...app, languages: ["go"] }], noApp: null },
    { state: "ready", items: [{ ...app, extra: 1 }], noApp: null },
    { state: "ready", items: [(({ fileCount, ...rest }) => rest)(app)], noApp: null },
    { state: "ready", items: [] },
  ]) assert.equal(apps(bad), false, JSON.stringify(bad));
});

// #851: `trellis_endpoints` takes bounded filters and pages closed, structural rows.
test("trellis_endpoints input is closed and its rows are structural evidence", () => {
  const endpoints = tools.find(tool => tool.name === "trellis_endpoints");
  assert.deepEqual(endpoints.annotations, { destructiveHint: false, idempotentHint: true, openWorldHint: false, readOnlyHint: true });
  assert.ok(!JSON.stringify(endpoints).includes("$schema"));
  const input = ajv.compile(endpoints.inputSchema);
  for (const ok of [{}, { app: "web" }, { method: "GET", pathTemplate: "/api/items/{id}" }, { limit: 50, offset: 3 },
    { workspace: "/repo", expectedBasis: { indexGeneration: "123e4567-e89b-42d3-a456-426614174000", indexRevision: 1 } }])
    assert.ok(input({ schemaVersion: 1, ...ok }), JSON.stringify([ok, input.errors]));
  for (const bad of [{ method: "get" }, { method: "TRACE" }, { pathTemplate: "" }, { limit: 0 }, { limit: 51 },
    { offset: -1 }, { list: "matched" }, { app: "" }])
    assert.equal(input({ schemaVersion: 1, ...bad }), false, JSON.stringify(bad));
  const output = ajv.compile(endpoints.outputSchema);
  const site = { path: "web/app.js", lines: { start: 3, end: 3 }, confidence: "exact",
    caller: { name: "load", stableLookupKey: "web/app.js::load" }, app: "web" };
  const handler = { kind: "declaration", syntaxId: `sid:v1:${"a".repeat(32)}`, stableLookupKey: "src/main.rs::health",
    path: "src/main.rs", lines: { start: 1, end: 1 }, variant: null };
  const rows = [
    { list: "matched", method: "GET", pathTemplate: "/health", app: "srv", handler, callSites: [site], callSiteCount: 6 },
    { list: "serverOnly", method: "POST", pathTemplate: "/orphan", app: "srv", handler, callSites: [], callSiteCount: 0 },
    { list: "clientOnly", method: null, pathTemplate: null, app: null, handler: null,
      callSites: [{ ...site, confidence: "unresolved", caller: null, app: null }], callSiteCount: 1 },
  ];
  const envelope = (data) => ({ schemaVersion: 1, requestId: 1, workspace: "/repo", catchingUp: false,
    evidenceBasis: { indexGeneration: "123e4567-e89b-42d3-a456-426614174000", indexRevision: 1 },
    data: { evidence: "structural", extracted: true, limitations: ["l"], endpoints: rows,
      indexGeneration: "123e4567-e89b-42d3-a456-426614174000", indexRevision: 1,
      page: { offset: 0, nextOffset: null, total: 3 }, ...data },
    warnings: [], partial: true, truncated: false, truncationReason: null });
  assert.ok(output(envelope({})), JSON.stringify(output.errors));
  assert.ok(output(envelope({ extracted: false, endpoints: [], limitations: ["l", "not extracted: fewer than two apps"] })));
  for (const bad of [
    { evidence: "semantic" },
    { endpoints: [{ ...rows[0], list: "unknown" }] },
    { endpoints: [{ ...rows[0], callSites: [{ ...site, confidence: "semantic" }] }] },
    { endpoints: [{ ...rows[0], handler: { ...handler, layer: "web" } }] },
    { endpoints: [(({ callSiteCount, ...rest }) => rest)(rows[0])] },
  ]) assert.equal(output(envelope(bad)), false, JSON.stringify(bad));
});
