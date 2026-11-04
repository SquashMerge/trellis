import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync, mkdtempSync, writeFileSync, chmodSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawnSync} from 'node:child_process';
import {createRequire} from 'node:module';
import {exercise, parseArgs} from '../tools/mcp-smoke.mjs';

const require = createRequire(new URL('./mcp/package.json', import.meta.url));
const tools = JSON.parse(readFileSync(new URL('./fixtures/mcp/catalog.json', import.meta.url)));
const basis = {indexGeneration: '11111111-1111-1111-1111-111111111111', indexRevision: 1};
const declarationBasis = {...basis, indexRevision: 2};
const rulesBasis = {...basis, indexRevision: 3};
const identity = {sourceSetId: 'fixture', language: 'rust', path: 'src/main.rs', sid: `sid:v1:${'a'.repeat(32)}`};
const destination = {...identity, path: 'src/core.rs'};
const document = {ref: 7, document: {sourceSetId: 'fixture', language: 'rust', path: 'src/main.rs'}, contentHash: 'a'.repeat(64)};
const symbol = {kind: 'function', symbolId: identity.sid, doc: 7};
const instance = {rule: '$layers', target: identity, destination};
function response(name) {
  let data = {};
  let evidenceBasis = basis;
  if (name === 'trellis_workspace_describe') data = {indexState: 'ready', watcherDegraded: false, languages: [{language: 'rust', semantic: {producerId: 'rust-analyzer', available: true, freshness: 'fresh'}}], searchIndex: {unavailableReason: null}};
  if (name === 'trellis_find_symbols') data = {symbols: [symbol], documents: [document], scopeCoverage: [{language: 'rust', semantic: {producerId: 'rust-analyzer', available: true, freshness: 'fresh'}}]};
  if (name === 'trellis_declaration') {
    data = {doc: 7, documents: [{...document, document: {...document.document, path: 'src/discovered.rs'}}], lines: {start: 10, end: 40}};
    evidenceBasis = declarationBasis;
  }
  if (name === 'trellis_rules_check') { data = {instances: [instance], testOnly: [], page: {nextOffset: null}}; evidenceBasis = rulesBasis; }
  if (name === 'trellis_search_symbols') data = {searchIndex: {state: 'complete', unavailableReason: null}};
  return {content: [{type: 'text', text: 'SECRET content must never be printed'}], structuredContent: {liveMarker: true, evidenceBasis, data}};
}
// Deliberately different live output schemas: the smoke must use advertised
// schemas, not load the checked-in catalog to validate server responses.
const liveTools = tools.map(tool => ({...tool, outputSchema: {
  type: 'object', required: ['liveMarker', 'data'], properties: {liveMarker: {const: true}, data: {type: 'object'}},
}}));
function clientFor(transform = (_, result) => result) {
  const calls = [];
  return {calls, async request(request) {
    const {name, arguments: args} = request.params;
    calls.push({name, args});
    return transform(name, response(name), args);
  }};
}

test('all eleven live tools use discovered inputs, declaration refs and pinned rule identity', async () => {
  const client = clientFor();
  const report = await exercise(client, liveTools, '/fixture', 'fixtureMain');
  assert.equal(report.ok, true, JSON.stringify(report));
  assert.deepEqual(new Set(client.calls.map(call => call.name)), new Set(tools.map(tool => tool.name)));
  const source = client.calls.find(call => call.name === 'trellis_read_source').args;
  assert.deepEqual(source, {schemaVersion: 1, workspace: '/fixture', expectedBasis: declarationBasis,
    path: 'src/discovered.rs', expectedContentHash: document.contentHash, startLine: 10, endLine: 14});
  const explain = client.calls.find(call => call.name === 'trellis_rules_explain').args;
  assert.deepEqual(explain, {schemaVersion: 1, workspace: '/fixture', expectedBasis: rulesBasis,
    rule: instance.rule, target: identity, destination, limit: 1});
  for (const call of client.calls.filter(call => ['trellis_outgoing_calls', 'trellis_incoming_calls', 'trellis_call_graph'].includes(call.name))) {
    assert.equal(call.args.symbolId, symbol.symbolId);
    assert.equal(call.args.compact, undefined);
    assert.deepEqual(call.args.expectedBasis, basis);
  }
  assert.ok(report.results.every(row => row.elapsedMs >= 0 && row.resultBytes > 0));
  assert.ok(!JSON.stringify(report).includes('SECRET'));
});

test('schema-valid typed failures remain failures and every catalog recipe is accounted for', async () => {
  const client = clientFor((name) => ({isError: true, content: [{type: 'text', text: 'SECRET'}], structuredContent: {
    schemaVersion: 1, requestId: 1, workspace: '/fixture', catchingUp: false,
    error: {code: 'deadline_exceeded', message: 'SECRET token', retryable: true, currentBasis: null, currentContentHash: null},
  }}));
  const report = await exercise(client, tools, '/fixture', 'main');
  assert.equal(report.ok, false);
  assert.equal(new Set(report.results.map(row => row.tool)).size, tools.length);
  assert.ok(report.results.filter(row => row.reason === 'tool_error').length >= 5);
  assert.ok(report.results.filter(row => row.reason === 'tool_error').every(row => row.code === 'deadline_exceeded'));
  assert.ok(!JSON.stringify(report).includes('SECRET'));
});

test('live schema mismatches, thrown transport errors and lexical-only search fail without hiding unrelated calls', async () => {
  for (const mode of ['schema', 'transport', 'model']) {
    const client = clientFor((name, result) => {
      if (name === 'trellis_outgoing_calls' && mode === 'schema') delete result.structuredContent.liveMarker;
      if (name === 'trellis_outgoing_calls' && mode === 'transport') throw Error('SECRET token');
      if (name === 'trellis_search_symbols' && mode === 'model') result.structuredContent.data.searchIndex.unavailableReason = 'model_not_configured';
      return result;
    });
    const report = await exercise(client, liveTools, '/fixture', 'main');
    assert.equal(report.ok, false);
    const failed = report.results.filter(row => row.status === 'fail');
    assert.equal(failed.length, 1, JSON.stringify(report));
    assert.equal(failed[0].reason, {schema: 'output_schema_mismatch', transport: 'transport_error', model: 'missing_search_capability'}[mode]);
    assert.equal(client.calls.length, tools.length);
    assert.ok(!JSON.stringify(report).includes('SECRET'));
  }
});

test('rules discovery pages testOnly and omits a null destination', async () => {
  const client = clientFor((name, result, args) => {
    if (name === 'trellis_rules_check') result.structuredContent.data = args.offset
      ? {instances: [], testOnly: [{...instance, destination: null}], page: {nextOffset: null}}
      : {instances: [], testOnly: [], page: {nextOffset: 5}};
    return result;
  });
  const report = await exercise(client, liveTools, '/fixture', 'main');
  assert.equal(report.ok, true, JSON.stringify(report));
  assert.equal(client.calls.filter(call => call.name === 'trellis_rules_check').length, 2);
  assert.equal('destination' in client.calls.find(call => call.name === 'trellis_rules_explain').args, false);
});

test('missing symbols, empty rules, and unknown advertised tools are explicit failures, not fake calls', async () => {
  const client = clientFor((name, result) => {
    if (name === 'trellis_find_symbols') result.structuredContent.data.symbols = [];
    if (name === 'trellis_rules_check') result.structuredContent.data.instances = [];
    return result;
  });
  const report = await exercise(client, [...liveTools, {...liveTools[0], name: 'SECRET unknown tool'}], '/fixture', 'main');
  assert.equal(report.ok, false);
  assert.ok(report.results.some(row => row.reason === 'missing_callable_symbol'));
  assert.ok(report.results.some(row => row.reason === 'missing_rule_instance'));
  assert.ok(report.results.some(row => row.reason === 'missing_tool_recipe'));
  assert.ok(!JSON.stringify(report).includes('SECRET'));
  assert.ok(!client.calls.some(call => call.name === 'trellis_rules_explain'));
});

test('installed-binary stdio entry point suppresses child stderr, tool text and exception text', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'trellis-mcp-smoke-test-'));
  try {
    const binary = join(temporary, 'installed-trellis');
    const fixture = join(temporary, 'responses.json');
    writeFileSync(fixture, JSON.stringify({tools: liveTools, responses: Object.fromEntries(tools.map(tool => [tool.name, response(tool.name)]))}));
    writeFileSync(binary, `#!/usr/bin/env node
const {createInterface} = require('node:readline');
const fixture = JSON.parse(require('node:fs').readFileSync(${JSON.stringify(fixture)}));
if (JSON.stringify(process.argv.slice(2)) !== JSON.stringify(['mcp','--workspace',${JSON.stringify(temporary)}])) process.exit(3);
process.stderr.write('SECRET token\\n');
createInterface({input: process.stdin}).on('line', line => {
 const request = JSON.parse(line);
 if (!('id' in request)) return;
 let result;
 if (request.method === 'initialize') result = {protocolVersion:'2025-11-25',capabilities:{tools:{}},serverInfo:{name:'fixture',version:'1'}};
 if (request.method === 'tools/list') result = {tools:fixture.tools};
 if (request.method === 'tools/call') result = fixture.responses[request.params.name];
 process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:request.id,result})+'\\n');
});
`);
    chmodSync(binary, 0o755);
    const result = spawnSync(fileURLToPath(new URL('../tools/mcp-smoke', import.meta.url)), ['--binary', binary, '--workspace', temporary], {encoding: 'utf8', timeout: 20_000});
    assert.equal(result.status, 0, result.stdout + result.stderr);
    const report = JSON.parse(result.stdout);
    assert.equal(report.ok, true);
    assert.equal(report.results.length, tools.length);
    for (const loads of Object.values(report.loadAverage)) {
      assert.equal(loads.length, 3);
      assert.ok(loads.every(value => Number.isFinite(value) && value >= 0));
    }
    assert.equal(result.stderr, '');
    assert.ok(!result.stdout.includes('SECRET'));
  } finally { rmSync(temporary, {recursive: true, force: true}); }
});


test('known unavailable capabilities fail while watcher degradation and native partial coverage do not', async () => {
  for (const mode of ['search_unavailable', 'search_building', 'semantic_missing', 'semantic_stale', 'indexing', 'watcher']) {
    const client = clientFor((name, result) => {
      const data = result.structuredContent.data;
      if (name === 'trellis_search_symbols' && mode.startsWith('search_')) data.searchIndex.state = mode === 'search_building' ? 'building' : 'unavailable';
      if (name === 'trellis_find_symbols' && mode === 'semantic_missing') data.scopeCoverage[0].semantic.available = false;
      if (name === 'trellis_find_symbols' && mode === 'semantic_stale') data.scopeCoverage[0].semantic.freshness = 'stale';
      if (name === 'trellis_workspace_describe' && mode === 'indexing') data.indexState = 'indexing';
      if (name === 'trellis_workspace_describe' && mode === 'watcher') data.watcherDegraded = true;
      return result;
    });
    const report = await exercise(client, liveTools, '/fixture', 'main');
    assert.equal(report.ok, mode === 'watcher', `${mode}: ${JSON.stringify(report)}`);
    assert.equal(report.catalogTools, tools.length);
    assert.equal(report.distinctToolsCalled, tools.length);
    assert.equal(report.blockedTools, 0);
    assert.equal(report.calls, tools.length);
    if (mode === 'watcher') assert.equal(report.results[0].watcherDegraded, true);
    if (mode === 'search_building') {
      assert.equal(report.results.find(row => row.tool === 'trellis_search_symbols').reason, 'search_coverage_building');
    }
  }
});

test('an arbitrary error code advertised in the live schema never reaches the report', async () => {
  const secretTools = liveTools.map(tool => ({...tool, outputSchema: {...tool.outputSchema,
    $defs: {SecretCode: {enum: ['SECRET_TOKEN']}}}}));
  const client = clientFor((name, result) => {
    if (name === 'trellis_outgoing_calls') result.structuredContent.error = {code: 'SECRET_TOKEN'};
    return result;
  });
  const report = await exercise(client, secretTools, '/fixture', 'main');
  assert.equal(report.ok, false);
  assert.equal(report.results.find(row => row.reason === 'tool_error').code, 'other');
  assert.ok(!JSON.stringify(report).includes('SECRET_TOKEN'));
});

test('probe prefers a configured language and reports unconfigured languages without failure', async () => {
  const pythonDocument = {...document, ref: 8, document: {...document.document, language: 'python', path: 'tools/main.py'}};
  const client = clientFor((name, result) => {
    const data = result.structuredContent.data;
    const python = {language: 'python', semantic: {producerId: null, available: false, freshness: 'unavailable'}};
    if (name === 'trellis_workspace_describe') data.languages.unshift(python);
    if (name === 'trellis_find_symbols') {
      data.symbols.unshift({...symbol, doc: 8, symbolId: `sid:v1:${'b'.repeat(32)}`});
      data.documents.unshift(pythonDocument);
      data.scopeCoverage.unshift(python);
    }
    return result;
  });
  const report = await exercise(client, liveTools, '/fixture', 'main');
  assert.equal(report.ok, true, JSON.stringify(report));
  assert.equal(client.calls.find(call => call.name === 'trellis_declaration').args.symbolId, symbol.symbolId);
  assert.deepEqual(report.semanticCapabilities.find(row => row.language === 'python'), {language: 'python', status: 'not_configured'});
});

test('a language without a selected producer is reported as not configured', async () => {
  const client = clientFor((name, result) => {
    const rows = name === 'trellis_workspace_describe' ? result.structuredContent.data.languages
      : name === 'trellis_find_symbols' ? result.structuredContent.data.scopeCoverage : [];
    for (const row of rows) row.semantic = {producerId: null, available: false, freshness: 'unavailable'};
    return result;
  });
  const report = await exercise(client, liveTools, '/fixture', 'main');
  assert.equal(report.ok, true, JSON.stringify(report));
  assert.equal(report.semanticCapabilities.find(row => row.language === 'rust').status, 'not_configured');
});

test('a configured producer missing from scope still fails even when another language is fresh', async () => {
  for (const mode of ['missing', 'stale']) {
    const client = clientFor((name, result) => {
      if (name === 'trellis_workspace_describe') result.structuredContent.data.languages.push({language: 'javascript', semantic: {producerId: 'scip-typescript', available: true, freshness: 'fresh'}});
      if (name === 'trellis_find_symbols') result.structuredContent.data.scopeCoverage.push({language: 'javascript', semantic: {producerId: mode === 'missing' ? null : 'scip-typescript', available: false, freshness: mode === 'missing' ? 'unavailable' : 'stale'}});
      return result;
    });
    const report = await exercise(client, liveTools, '/fixture', 'main');
    assert.equal(report.ok, false, JSON.stringify(report));
    assert.ok(report.results.some(row => row.reason === 'missing_semantic_capability'));
  }
});

test('CLI requires explicit absolute installed binary and checkout', () => {
  assert.deepEqual(parseArgs(['--binary', '/installed/trellis', '--workspace', '/fixture']), {binary: '/installed/trellis', workspace: '/fixture', query: 'main'});
  assert.throws(() => parseArgs(['--binary', 'trellis', '--workspace', '/fixture']));
  assert.throws(() => parseArgs(['--workspace', '/fixture']));
});
