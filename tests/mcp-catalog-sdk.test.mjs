// The MCP catalog must be a tools/list result that real MCP clients accept.
// Validate the exact catalog fixture (which the Rust catalog must equal) with
// the pinned MCP TypeScript SDK's ListToolsResultSchema (#158).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(new URL("../runtime/acp/package.json", import.meta.url));
const { ListToolsResultSchema } = require("@modelcontextprotocol/sdk/types.js");
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
