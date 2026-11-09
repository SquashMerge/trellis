import {createRequire} from 'node:module';
import {isAbsolute, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {access, constants} from 'node:fs/promises';
import {loadavg} from 'node:os';

const require = createRequire(new URL('../tests/mcp/package.json', import.meta.url));
const names = [
  'trellis_workspace_describe', 'trellis_find_symbols', 'trellis_search_symbols',
  'trellis_declaration', 'trellis_outgoing_calls', 'trellis_incoming_calls',
  'trellis_read_source', 'trellis_call_graph', 'trellis_references',
  'trellis_rules_check', 'trellis_rules_explain',
];
const promptName = 'trellis_propose_layers';
const callableKinds = new Set(['function', 'method', 'constructor']);
const failureCodes = new Set(['invalid_request', 'range_too_large', 'unsupported_encoding',
  'revision_conflict', 'index_not_ready', 'not_found', 'too_many_requests', 'deadline_exceeded',
  'rebuild_in_progress', 'root_changed', 'store_unavailable', 'workspace_selection_failed',
  'checkout_capacity', 'daemon_unavailable', 'outcome_unknown', 'invalid_rules',
  'rules_not_found', 'bounded_work', 'response_too_large', 'storage_busy', 'building']);
const modelReasons = new Set(['model_not_configured', 'model_load_failed', 'vector_budget_exceeded']);
// #798: an architecture tool answers the typed `building` while the daemon computes the
// revision. The smoke reads again after `retryAfterMs`, for at most this long per call.
const buildingBudgetMs = 60_000;
const defaultSleep = ms => new Promise(done => setTimeout(done, ms));
function retryAfter(error) {
  const ms = error?.retryAfterMs;
  return Number.isSafeInteger(ms) && ms >= 0 ? Math.min(ms, 5_000) : 250;
}

export function dependencies() {
  return {
    Client: require('@modelcontextprotocol/sdk/client/index.js').Client,
    StdioClientTransport: require('@modelcontextprotocol/sdk/client/stdio.js').StdioClientTransport,
    CallToolResultSchema: require('@modelcontextprotocol/sdk/types.js').CallToolResultSchema,
    GetPromptResultSchema: require('@modelcontextprotocol/sdk/types.js').GetPromptResultSchema,
    Ajv2020: require('ajv/dist/2020.js').default,
  };
}

// Only closed local labels reach the report. Never print repository values,
// server messages, validator errors, exceptions, or the child's stderr.
export async function exercise(client, tools, workspace, query, deps = dependencies()) {
  const ajv = new deps.Ajv2020({strict: false, allErrors: true});
  const sleep = deps.sleep ?? defaultSleep;
  const buildingBudget = deps.buildingBudgetMs ?? buildingBudgetMs;
  const results = [];
  let calls = 0;
  const called = new Set();
  const catalog = new Map();
  for (const tool of tools) {
    const toolName = names.includes(tool.name) ? tool.name : 'unknown_tool';
    if (catalog.has(tool.name)) {
      results.push({tool: toolName, status: 'fail', reason: 'duplicate_catalog_tool'});
      continue;
    }
    try {
      if (!tool.inputSchema || !tool.outputSchema) throw new Error();
      catalog.set(tool.name, {
        input: ajv.compile(tool.inputSchema), output: ajv.compile(tool.outputSchema),
      });
    } catch {
      catalog.set(tool.name, null);
      results.push({tool: toolName, status: 'fail', reason: 'catalog_schema_invalid'});
    }
    if (toolName === 'unknown_tool') {
      results.push({tool: toolName, status: 'fail', reason: 'missing_tool_recipe'});
    }
  }
  for (const name of names) {
    if (!catalog.has(name)) results.push({tool: name, status: 'fail', reason: 'missing_catalog_tool'});
  }
  function missing(name, reason) {
    if (catalog.get(name)) results.push({tool: name, status: 'fail', reason, elapsedMs: 0});
  }
  async function call(name, args) {
    const schema = catalog.get(name);
    if (!schema) return null;
    const input = {schemaVersion: 1, workspace, ...args};
    const started = performance.now();
    const row = {tool: name, status: 'fail', reason: 'transport_error', elapsedMs: 0};
    results.push(row);
    try {
      if (!schema.input(input)) {
        row.reason = 'input_schema_mismatch';
        return null;
      }
      // request() leaves output validation here, so even isError results are checked.
      calls += 1;
      called.add(name);
      let result;
      for (;;) {
        result = await client.request({method: 'tools/call', params: {name, arguments: input}},
          deps.CallToolResultSchema);
        const error = result.structuredContent?.error;
        if (error?.code !== 'building' || performance.now() - started >= buildingBudget) break;
        row.buildingRetries = (row.buildingRetries ?? 0) + 1;
        await sleep(retryAfter(error));
      }
      row.resultBytes = Buffer.byteLength(JSON.stringify(result));
      const value = result.structuredContent;
      if (!schema.output(value)) {
        row.reason = 'output_schema_mismatch';
        return null;
      }
      if (value.error) {
        row.reason = 'tool_error';
        row.code = failureCodes.has(value.error.code) ? value.error.code : 'other';
        return null;
      }
      if (result.isError) {
        row.reason = 'mcp_error';
        return null;
      }
      if (row.resultBytes > 65_536) {
        row.reason = 'response_too_large';
        return null;
      }
      row.status = 'pass';
      delete row.reason;
      const reason = value.data?.searchIndex?.unavailableReason;
      if (modelReasons.has(reason) || name === 'trellis_search_symbols' && value.data.searchIndex.state !== 'complete') {
        row.status = 'fail';
        row.reason = value.data.searchIndex.state === 'building' || value.data.searchIndex.state === 'partial'
          ? 'search_coverage_building' : 'missing_search_capability';
        if (modelReasons.has(reason)) {
          row.reason = 'missing_search_capability';
          row.code = reason;
        }
      }
      if (name === 'trellis_workspace_describe') row.watcherDegraded = value.data.watcherDegraded === true;
      if (name === 'trellis_workspace_describe' && value.data.indexState !== 'ready') {
        row.status = 'fail';
        row.reason = 'workspace_not_ready';
      }
      return value;
    } catch (error) {
      // SDK timeout code, not an untrusted exception string.
      if (error?.code === -32001) row.reason = 'client_timeout';
      return null;
    } finally {
      row.elapsedMs = Math.round((performance.now() - started) * 100) / 100;
    }
  }
  // #770: the no-argument proposal prompt must fit the frame on a real checkout.
  async function prompt() {
    const started = performance.now();
    const row = {tool: promptName, kind: 'prompt', status: 'fail', reason: 'transport_error', elapsedMs: 0};
    results.push(row);
    try {
      let result;
      for (;;) {
        try {
          result = await client.request({method: 'prompts/get', params: {name: promptName, arguments: {}}},
            deps.GetPromptResultSchema);
          break;
        } catch (error) {
          if (error?.code !== -32603 || error.data?.code !== 'building'
            || performance.now() - started >= buildingBudget) throw error;
          row.buildingRetries = (row.buildingRetries ?? 0) + 1;
          await sleep(retryAfter(error.data));
        }
      }
      row.resultBytes = Buffer.byteLength(JSON.stringify(result));
      if (typeof result.messages?.[0]?.content?.text !== 'string') {
        row.reason = 'prompt_shape_invalid';
      } else if (row.resultBytes > 65_536) {
        row.reason = 'response_too_large';
      } else {
        row.status = 'pass';
        delete row.reason;
      }
    } catch (error) {
      if (error?.code === -32001) row.reason = 'client_timeout';
      if (error?.code === -32603) {
        row.reason = 'prompt_error';
        const code = error.data?.code;
        row.code = failureCodes.has(code) ? code : 'other';
      }
    } finally {
      row.elapsedMs = Math.round((performance.now() - started) * 100) / 100;
    }
  }
  const describe = await call('trellis_workspace_describe', {});
  const basis = describe?.evidenceBasis;
  const pin = basis ? {expectedBasis: basis} : {};
  const found = await call('trellis_find_symbols', {...pin, query, limit: 20});
  const describedLanguages = describe?.data?.languages ?? [];
  const scopeLanguages = found?.data?.scopeCoverage ?? [];
  const configuredLanguages = new Set([...describedLanguages, ...scopeLanguages]
    .filter(row => row.semantic?.producerId).map(row => row.language));
  const semanticCapabilities = ['java', 'rust', 'python', 'javascript', 'typescript'].map(language => {
    // #715: find's `scopeCoverage` lists only the returned languages; describe covers the rest.
    const semantic = (scopeLanguages.find(row => row.language === language)
      ?? describedLanguages.find(row => row.language === language))?.semantic;
    const configured = configuredLanguages.has(language);
    const ready = semantic?.producerId && semantic.available && semantic.freshness === 'fresh';
    return {language, status: !configured ? 'not_configured' : ready ? 'ready' : 'unavailable'};
  });
  if (semanticCapabilities.some(row => row.status === 'unavailable')) {
    results.push({tool: 'trellis_workspace_describe', status: 'fail', reason: 'missing_semantic_capability', elapsedMs: 0});
  }
  const documents = found?.data?.documents ?? [];
  const callables = (found?.data?.symbols ?? []).filter(item => callableKinds.has(item.kind));
  const symbol = callables.find(item => configuredLanguages.has(
    documents.find(document => document.ref === item.doc)?.document.language)) ?? callables[0];
  const symbolBasis = found?.evidenceBasis;
  const symbolPin = symbolBasis ? {expectedBasis: symbolBasis} : pin;
  const document = documents.find(item => item.ref === symbol?.doc);
  await call('trellis_search_symbols', {...symbolPin, query, limit: 1});
  if (symbol) {
    const selected = {...symbolPin, symbolId: symbol.symbolId};
    const declaration = await call('trellis_declaration', selected);
    const sourceDocument = declaration?.data?.documents?.find(item => item.ref === declaration.data.doc);
    if (sourceDocument && declaration.data.lines) {
      const lines = declaration.data.lines;
      await call('trellis_read_source', {
        expectedBasis: declaration.evidenceBasis, path: sourceDocument.document.path,
        expectedContentHash: sourceDocument.contentHash,
        startLine: lines.start, endLine: Math.min(lines.end, lines.start + 4),
      });
    } else {
      missing('trellis_read_source', 'missing_source_location');
    }
    await call('trellis_outgoing_calls', {...selected, limit: 1});
    await call('trellis_incoming_calls', {...selected, limit: 1});
    await call('trellis_call_graph', {...selected, depth: 1, maxNodes: 10, maxCalls: 10});
  } else {
    for (const name of ['trellis_declaration', 'trellis_read_source', 'trellis_outgoing_calls', 'trellis_incoming_calls', 'trellis_call_graph']) {
      missing(name, 'missing_callable_symbol');
    }
  }
  await call('trellis_references', {...symbolPin, ...(document ? {paths: [document.document.path]} : {}), limit: 1});
  let checked = await call('trellis_rules_check', {...symbolPin, limit: 5});
  let instance;
  let offset = 0;
  while (checked) {
    instance = [...checked.data.instances, ...checked.data.testOnly][0];
    if (instance) break;
    const next = checked.data.page.nextOffset;
    if (next === null) break;
    if (!Number.isSafeInteger(next) || next <= offset) {
      missing('trellis_rules_explain', 'invalid_page_progress');
      return {ok: false, results};
    }
    offset = next;
    checked = await call('trellis_rules_check', {...symbolPin, limit: 5, offset});
  }
  if (instance) {
    await call('trellis_rules_explain', {
      expectedBasis: checked.evidenceBasis, rule: instance.rule, target: instance.target,
      ...(instance.destination ? {destination: instance.destination} : {}), limit: 1,
    });
  } else {
    missing('trellis_rules_explain', 'missing_rule_instance');
  }
  await prompt();
  return {ok: results.length > 0 && results.every(row => row.status === 'pass'),
    catalogTools: tools.length, calls, distinctToolsCalled: called.size, semanticCapabilities,
    blockedTools: [...catalog.keys()].filter(name => !called.has(name)).length, results};
}

export function parseArgs(args) {
  const options = {query: 'main'};
  for (let i = 0; i < args.length; i += 2) {
    const flag = args[i];
    if (!['--binary', '--workspace', '--query'].includes(flag) || !args[i + 1] || options[flag.slice(2)] !== undefined && flag !== '--query') {
      throw new Error('usage');
    }
    options[flag.slice(2)] = args[i + 1];
  }
  if (!isAbsolute(options.binary ?? '') || !isAbsolute(options.workspace ?? '')) throw new Error('usage');
  return options;
}

export async function main(args) {
  let options;
  try {
    options = parseArgs(args);
    await access(options.binary, constants.X_OK);
    await access(options.workspace, constants.R_OK);
  } catch {
    console.log(JSON.stringify({ok: false, reason: 'usage', usage: 'tools/mcp-smoke --binary /absolute/installed/trellis --workspace /absolute/checkout [--query callable-name]'}));
    return 2;
  }
  let deps;
  try { deps = dependencies(); } catch {
    console.log(JSON.stringify({ok: false, reason: 'dependencies_missing', setup: 'npm ci --prefix tests/mcp --ignore-scripts'}));
    return 2;
  }
  const client = new deps.Client({name: 'trellis-installed-smoke', version: '1.0.0'}, {capabilities: {}});
  const transport = new deps.StdioClientTransport({
    command: options.binary, args: ['mcp', '--workspace', options.workspace],
    cwd: options.workspace, env: {...process.env}, stderr: 'ignore',
  });
  let report;
  const started = performance.now();
  const loadBefore = loadavg();
  try {
    await client.connect(transport);
    const tools = [];
    let cursor;
    const seen = new Set();
    do {
      const page = await client.listTools(cursor ? {cursor} : {});
      tools.push(...page.tools);
      cursor = page.nextCursor;
      if (cursor && seen.has(cursor)) throw new Error();
      seen.add(cursor);
    } while (cursor);
    report = await exercise(client, tools, options.workspace, options.query, deps);
  } catch {
    report = {ok: false, reason: 'connection_or_catalog_error'};
  } finally {
    try { await client.close(); } catch {
      report = {...report, ok: false, reason: 'close_error'};
    }
  }
  report.elapsedMs = Math.round((performance.now() - started) * 100) / 100;
  report.loadAverage = {before: loadBefore, after: loadavg()};
  console.log(JSON.stringify(report, null, 2));
  return report.ok ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = await main(process.argv.slice(2));
}
