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
  assert.equal(tools.length, 10);
  assert.ok(!tools.some(tool => tool.name === "trellis_propose_layers"));
});

// Draft 2020-12 is the advertised wire schema, rather than an SDK-only shape check.
const Ajv2020 = require("ajv/dist/2020.js").default;
const ajv = new Ajv2020({ strict: false });
const search = tools.find(tool => tool.name === "trellis_search_symbols");

test("the ten-tool order and unchanged schemas are explicit", () => {
  assert.deepEqual(tools.map(tool => tool.name), [
    "trellis_workspace_describe", "trellis_find_symbols", "trellis_search_symbols",
    "trellis_declaration", "trellis_outgoing_calls", "trellis_incoming_calls",
    "trellis_read_source", "trellis_references", "trellis_rules_check", "trellis_rules_explain",
  ]);
  const describe = tools[0].outputSchema.oneOf[0].properties.data.properties;
  assert.equal(describe.toolVersion.const, 2);
  assert.equal(search.inputSchema.additionalProperties, false);
  assert.equal(search.inputSchema.properties.limit.default, 10);
  assert.equal(search.inputSchema.properties.limit.maximum, 50);
  const findSymbol = tools[1].outputSchema.$defs.Symbol;
  const { rank, matchedBy, ...symbolFields } = search.outputSchema.$defs.SearchHit.properties;
  assert.deepEqual(symbolFields, findSymbol.properties);
  assert.deepEqual(search.outputSchema.$defs.SearchHit.required,
    [...findSymbol.required, "rank", "matchedBy"]);
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
    declarations: 3, embedded: 0, lexical: "complete" };
  for (const backend of [null, "cpu", "metal"]) {
    assert.ok(validateStatus({ ...status, backend }), JSON.stringify(validateStatus.errors));
  }
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
