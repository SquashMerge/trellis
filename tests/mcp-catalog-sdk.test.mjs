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
  const defs = tools.flatMap((tool) => Object.values(tool.outputSchema?.$defs ?? {}));
  for (const name of ["Symbol", "Declaration"]) {
    // Find Symbol uses symbolId; Declaration uses syntaxId.
    const matches = defs.filter((definition) => definition?.properties?.role &&
      (name === "Symbol" ? definition.properties.symbolId : definition.properties.syntaxId));
    assert.equal(matches.length, 1, name);
    assert.deepEqual(matches[0].properties.role.enum, ["test", "production"]);
    assert.ok(matches[0].required.includes("role"));
    assert.equal(matches[0].additionalProperties, false);
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
  assert.equal(tools.length, 9);
  assert.ok(!tools.some(tool => tool.name === "trellis_propose_layers"));
});
