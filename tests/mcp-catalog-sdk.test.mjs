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
  const prompts = [{name: "trellis_propose_layers", description: "Read-only layer proposal", arguments: []}];
  for (const result of [{prompts}, {resultType: "complete", ttlMs: 0, cacheScope: "private", prompts}]) {
    assert.ok(ListPromptsResultSchema.safeParse(result).success);
  }
  const messages = [{role: "user", content: {type: "text", text: "Propose a rules diff for human review"}}];
  for (const result of [{messages}, {resultType: "complete", messages}]) {
    assert.ok(GetPromptResultSchema.safeParse(result).success);
  }
  assert.equal(tools.length, 11);
  assert.ok(!tools.some(tool => tool.name === "trellis_propose_layers"));
});

// Draft 2020-12 is the advertised wire schema, rather than an SDK-only shape check.
const Ajv2020 = require("ajv/dist/2020.js").default;
const ajv = new Ajv2020({ strict: false });
const search = tools.find(tool => tool.name === "trellis_search_symbols");

test("the eleven-tool order and unchanged schemas are explicit", () => {
  assert.deepEqual(tools.map(tool => tool.name), [
    "trellis_workspace_describe", "trellis_find_symbols", "trellis_search_symbols",
    "trellis_declaration", "trellis_outgoing_calls", "trellis_incoming_calls",
    "trellis_read_source", "trellis_call_graph", "trellis_references", "trellis_rules_check",
    "trellis_rules_explain",
  ]);
  const describe = tools[0].outputSchema.oneOf[0].properties.data.properties;
  assert.equal(describe.toolVersion.const, 5);
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
  assert.equal(graph.inputSchema.properties.maxNodes.default, 150);
  assert.equal(graph.inputSchema.properties.maxCalls.default, 500);
});
