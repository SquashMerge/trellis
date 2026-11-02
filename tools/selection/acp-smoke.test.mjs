import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { safeEnv, OPTIONS, denyPermission, redactedDiagnostic, reportedUsd, groupIdentity, killGroup, permissionDecision, selectModel, errorEvidence, parseLimits, toolAudit, toolResponseAudit, sdkErrorCategory, buildPrompt } from './acp-smoke.mjs';
import { selectionTools, readPacket, providerPacket, LOCAL_PACKET_LIMIT, PROVIDER_PACKET_LIMIT } from './selection-mcp.mjs';
const HERE=path.dirname(fileURLToPath(import.meta.url));
const packet={schemaVersion:1,questionId:'dry',question:'What happens?',candidates:[{id:'c001',source:'untrusted'},{id:'c002',source:'data'}]};
const decisions=[{candidateId:'c001',relevance:'essential'},{candidateId:'c002',relevance:'uncertain'}];
function temp(t){const dir=fs.mkdtempSync(path.join(os.tmpdir(),'selection-test-'));t.after(()=>fs.rmSync(dir,{recursive:true,force:true}));return dir;}
test('environment uses allowlist and fixed output cap',()=>{
 const env=safeEnv({HOME:'/home/example',PATH:'/bin',JEV_KEY:'secret',ANTHROPIC_API_KEY:'secret',ANTHROPIC_AUTH_TOKEN:'secret',NODE_OPTIONS:'--inspect',CLAUDE_CODE_EXECUTABLE:'/evil',CLAUDE_CODE_MAX_OUTPUT_TOKENS:'999999',MAX_MCP_OUTPUT_TOKENS:'999999'});
 assert.deepEqual(env,{HOME:'/home/example',PATH:'/bin',CLAUDE_CODE_MAX_OUTPUT_TOKENS:'4096',MAX_MCP_OUTPUT_TOKENS:'64000'});
});
test('deny permissions, disable builtins/settings and pin budget and model',()=>{
 assert.deepEqual(denyPermission(),{outcome:{outcome:'cancelled'}});
 assert.deepEqual(OPTIONS.tools,[]);assert.deepEqual(OPTIONS.settingSources,[]);
 assert.equal(OPTIONS.maxBudgetUsd,3);assert.equal(OPTIONS.maxTurns,4);assert.equal(OPTIONS.model,'opus[1m]');assert.equal(OPTIONS.persistSession,false);assert.equal(OPTIONS.allowDangerouslySkipPermissions,false);
 assert.deepEqual(OPTIONS.allowedTools,['mcp__selection__get_candidates','mcp__selection__get_candidate','mcp__selection__emit_view']);
});
test('diagnostics never retain arbitrary credential or environment text',()=>{
 assert.equal(redactedDiagnostic('secret=abc https://x/?token=abc'),'[redacted adapter diagnostic]\n');
 assert.equal(redactedDiagnostic('authentication failed key abc'),'[redacted adapter diagnostic: authentication]\n');
});
test('cost requires explicit finite USD, never derives dollars from tokens',()=>{
 assert.equal(reportedUsd([{used:1000}]),null);assert.equal(reportedUsd([{cost:{amount:2,currency:'EUR'}}]),null);
 assert.equal(reportedUsd([{cost:{amount:NaN,currency:'USD'}},{cost:{amount:-1,currency:'USD'}}]),null);
 assert.equal(reportedUsd([{cost:{amount:.1,currency:'USD'}},{cost:{amount:.2,currency:'USD'}}]),.2);
});
test('tools reject arbitrary paths, unknown IDs, duplicate/missing/bad labels, extra keys',t=>{
 const out=path.join(temp(t),'decisions.json'),api=selectionTools(packet,out);
 assert.deepEqual(JSON.parse(api.call('get_candidates').content[0].text),packet);
 assert.equal(api.call('get_candidates').structuredContent,undefined);
 assert.equal(JSON.parse(api.call('get_candidate',{candidateId:'c001'}).content[0].text).candidate.id,'c001');
 for(const [name,args] of [['get_candidates',{path:'/etc/passwd'}],['get_candidate',{candidateId:'unknown'}],['emit_view',{decisions,output:'/etc/evil'}],['shell',{}],['emit_view',{decisions:decisions.slice(1)}],['emit_view',{decisions:[decisions[0],decisions[0]]}],['emit_view',{decisions:[{candidateId:'c001',relevance:'bad'},decisions[1]]}],['emit_view',{decisions:[{...decisions[0],prose:'bad'},decisions[1]]}]]) assert.throws(()=>api.call(name,args));
 assert.equal(fs.existsSync(out),false);
 api.call('emit_view',{decisions});assert.deepEqual(JSON.parse(fs.readFileSync(out)),{decisions});
 assert.throws(()=>api.call('emit_view',{decisions}));
});
test('read packet rejects oversized and duplicate candidate IDs',t=>{
 const p=path.join(temp(t),'packet.json');
 fs.writeFileSync(p,JSON.stringify({...packet,candidates:[packet.candidates[0],packet.candidates[0]]}));assert.throws(()=>readPacket(p));
 fs.writeFileSync(p,' '.repeat(LOCAL_PACKET_LIMIT+1));assert.throws(()=>readPacket(p));
});
test('MCP stdio real initialize/list/call smoke, without any inference',async t=>{
 const dir=temp(t),p=path.join(dir,'packet.json'),out=path.join(dir,'decisions.json');fs.writeFileSync(p,JSON.stringify(packet));
 const transport=new StdioClientTransport({command:process.execPath,args:[path.join(HERE,'selection-mcp.mjs'),p,out],env:safeEnv(),stderr:'pipe'});
 const client=new Client({name:'dry-test',version:'1.0.0'});t.after(()=>client.close());
 await client.connect(transport);
 const listed=await client.listTools();assert.deepEqual(listed.tools.map(t=>t.name),['get_candidates','get_candidate','emit_view']);
 const candidates=await client.callTool({name:'get_candidates',arguments:{}});
 assert.deepEqual(JSON.parse(candidates.content[0].text),packet);assert.equal(candidates.structuredContent,undefined);
 assert.equal((await client.callTool({name:'emit_view',arguments:{decisions:[]}})).isError,true);
 assert.equal(fs.existsSync(out),false);
 assert.equal((await client.callTool({name:'emit_view',arguments:{decisions}})).structuredContent.accepted,true);
 assert.deepEqual(JSON.parse(fs.readFileSync(out)),{decisions});
});

test('cleanup terminates an owned process group without an inference process',async()=>{
 const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{detached:true,stdio:'ignore',env:safeEnv()});
 await once(child,'spawn');
 const exited=once(child,'exit');
 const result=await killGroup(child,groupIdentity(child.pid));await exited;
 assert.equal(result.complete,true);
 assert.throws(()=>process.kill(-child.pid,0),{code:'ESRCH'});
});

test('group cleanup fails closed on leader identity loss before TERM or escalation',async()=>{
 const child={pid:123,exitCode:null,signalCode:null};
 for(const initial of [null,'changed']) {
  const calls=[];
  const result=await killGroup(child,'recorded',{observe:()=>initial,signal:(...args)=>calls.push(args)});
  assert.equal(result.complete,false);assert.deepEqual(calls,[[-123,0]]);
 }
 for(const changed of ['changed',null]) {
  let reads=0;const calls=[];
  const result=await killGroup(child,'recorded',{observe:()=>++reads===1?'recorded':changed,
   signal:(...args)=>calls.push(args),pause:async()=>{}});
  assert.equal(result.complete,false);assert.deepEqual(calls,[[-123,'SIGTERM'],[-123,0]]);
 }
 const calls=[];
 const result=await killGroup({...child,exitCode:0},'recorded',{observe:()=>assert.fail('already reaped'),
  signal:(pid,name)=>{calls.push([pid,name]);throw Object.assign(new Error(),{code:'ESRCH'});}});
 assert.equal(result.complete,true);assert.deepEqual(calls,[[-123,0]]);
});
test('group escalation signals only while the original leader still matches',async()=>{
 const calls=[];
 const result=await killGroup({pid:123,exitCode:null,signalCode:null},'recorded',{
  observe:()=> 'recorded',signal:(...args)=>calls.push(args),pause:async()=>{}});
 assert.equal(result.complete,true);assert.deepEqual(calls,[[-123,'SIGTERM'],[-123,'SIGKILL']]);
});

test('only exact experiment tools with validated arguments receive allow_once',()=>{
 const params=(name,rawInput)=>({toolCall:{name,rawInput},options:[{kind:'allow_always',optionId:'always'},{kind:'allow_once',optionId:'once'}]});
 for(const [name,args] of [['get_candidates',{}],['get_candidate',{candidateId:'c001'}],['emit_view',{decisions}]]) assert.deepEqual(permissionDecision(packet,params('mcp__selection__'+name,args)),{outcome:{outcome:'selected',optionId:'once'}});
 for(const [name,args] of [['Bash',{command:'evil'}],['mcp__evil__get_candidates',{}],['mcp__selection__get_candidates',{path:'/etc/passwd'}],['mcp__selection__get_candidate',{candidateId:'unknown'}],['mcp__selection__emit_view',{decisions:[]}],['mcp__selection__emit_view',{decisions,path:'/evil'}]]) assert.deepEqual(permissionDecision(packet,params(name,args)),denyPermission());
 const p=params('mcp__selection__get_candidates',{});p.toolCall._meta={claudeCode:{mcpServer:{name:'evil'}}};assert.deepEqual(permissionDecision(packet,p),denyPermission());
 assert.deepEqual(permissionDecision(packet,{toolCall:{name:'mcp__selection__get_candidates',rawInput:{}},options:[{kind:'allow_always',optionId:'always'}]}),denyPermission());
});
test('model explicitly selected and verified before inference, mismatch fails closed',async()=>{
 const session={sessionId:'dry',configOptions:[{id:'model',currentValue:'wrong',options:[{value:'opus[1m]'}]}]};let call;
 const agent={request:async(method,params)=>{call={method,params};return {configOptions:[{id:'model',currentValue:'opus[1m]'}]};}};
 await selectModel(agent,session);assert.deepEqual(call,{method:'session/set_config_option',params:{sessionId:'dry',configId:'model',value:'opus[1m]'}});
 await assert.rejects(()=>selectModel({request:async()=>({configOptions:[{id:'model',currentValue:'sonnet'}]})},session),{safeCategory:'model_mismatch'});
 await assert.rejects(()=>selectModel(agent,{...session,configOptions:[]}),{safeCategory:'model_unavailable'});
});
test('failure evidence keeps code/category without provider message or secrets',()=>{
 assert.deepEqual(errorEvidence({code:-32000,message:'secret'},'session'),{category:'auth_required',code:-32000,stage:'session'});
 assert.deepEqual(errorEvidence({code:'ENOENT',message:'secret'},'prompt'),{category:'missing_emitted_view',code:'ENOENT',stage:'prompt'});
 assert.equal(JSON.stringify(errorEvidence({message:'secret'},'prompt')).includes('secret'),false);
});

test('budget flags can reduce reservations but cannot raise caps or remove headroom',()=>{
 assert.deepEqual(parseLimits(),{reserveUsd:5,sdkBudgetUsd:3});
 assert.deepEqual(parseLimits(['--reserve-usd','2','--sdk-budget-usd','1']),{reserveUsd:2,sdkBudgetUsd:1});
 for(const flags of [['--reserve-usd','6'],['--sdk-budget-usd','4'],['--reserve-usd','3'],['--reserve-usd','0'],['--sdk-budget-usd','NaN'],['--sdk-budget-usd','Infinity'],['--other','1'],['--reserve-usd'],['--reserve-usd','5','--reserve-usd','4']]) assert.throws(()=>parseLimits(flags));
});
test('tool audit counts unique calls, not permission requests or updates, and keeps no arguments',()=>{
 const audit=toolAudit();
 audit.observe({sessionUpdate:'tool_call',toolCallId:'one',title:'mcp__selection__get_candidates',rawInput:{secret:'never'}});
 audit.observe({sessionUpdate:'tool_call',toolCallId:'one',title:'mcp__selection__get_candidates'});
 audit.observe({sessionUpdate:'tool_call_update',toolCallId:'one',rawOutput:'secret'});
 audit.observe({sessionUpdate:'tool_call',toolCallId:'two',title:'secret command'});
 assert.deepEqual(audit.snapshot(),{count:2,names:['mcp__selection__get_candidates','[other tool]']});
});
test('packet permits 128 candidates and enforces compact provider payload size',t=>{
 const p=path.join(temp(t),'packet.json');
 const big={...packet,candidates:Array.from({length:128},(_,i)=>({id:`c${i}`,source:'x'}))};
 fs.writeFileSync(p,JSON.stringify(big));assert.equal(readPacket(p).candidates.length,128);
 fs.writeFileSync(p,JSON.stringify({...big,candidates:[...big.candidates,{id:'overflow'}]}));assert.throws(()=>readPacket(p));
 assert.throws(()=>providerPacket({...packet,question:'x'.repeat(PROVIDER_PACKET_LIMIT)}));
});

test('full local packet emits only allowlisted compact source once; candidate lookup cannot expand parent source',t=>{
 const local={...packet,labels:['essential','supporting','incidental','uncertain'],sourceFiles:[{path:'file.js',text:'full source'}],humanRubric:'never send',candidates:packet.candidates.map(c=>({...c,source:'x'.repeat(120000),calls:[]}))};
 const file=path.join(temp(t),'packet.json');fs.writeFileSync(file,JSON.stringify(local));
 assert.equal(readPacket(file).candidates.length,2);
 const api=selectionTools(local,path.join(path.dirname(file),'out.json'));
 const emitted=JSON.parse(api.call('get_candidates').content[0].text);
 assert.equal(emitted.humanRubric,undefined);assert.equal(emitted.candidates[0].source,undefined);
 assert.deepEqual(emitted.sourceFiles,[{path:'file.js',text:'full source'}]);
 assert.equal(JSON.parse(api.call('get_candidate',{candidateId:'c001'}).content[0].text).candidate.source,undefined);
 assert.throws(()=>providerPacket({...local,sourceFiles:[{path:'big.js',text:'x'.repeat(PROVIDER_PACKET_LIMIT)}]}));
});

test('known SDK terminal failures are categorized without saving provider messages',()=>{
 assert.equal(sdkErrorCategory({message:'Reached maximum number of turns (4)'}),'error_max_turns');
 assert.equal(sdkErrorCategory({message:'Reached maximum budget ($2.5)'}),'error_max_budget_usd');
 assert.equal(sdkErrorCategory({data:{errorKind:'error_max_turns'},message:'secret'}),'error_max_turns');
 assert.equal(sdkErrorCategory({data:{errorKind:'secret'},message:'secret'}),null);
 assert.equal(sdkErrorCategory({message:'Reached maximum number of turns (4) secret'}),null);
 assert.deepEqual(errorEvidence({code:-32603,message:'Reached maximum number of turns (4)'},'prompt'),{category:'prompt_failed',code:-32603,stage:'prompt',sdkCategory:'error_max_turns'});
});
test('tool response audit retains byte counts and bounded markers, never text or paths',()=>{
 const audit=toolResponseAudit();
 const update={sessionUpdate:'tool_call_update',toolCallId:'one',status:'completed',_meta:{claudeCode:{toolName:'mcp__selection__get_candidates'}},rawOutput:[{type:'text',text:'<persisted-output>Full output saved to: /secret/path</persisted-output>'}]};
 audit.observe(update);audit.observe(update);
 audit.observe({...update,toolCallId:'two',rawOutput:'[OUTPUT TRUNCATED - exceeded 25000 token limit]'});
 const result=audit.snapshot();assert.equal(result.count,2);assert.equal(result.responses[0].markers.persistedOutput,true);assert.equal(result.responses[0].markers.savedToFile,true);assert.equal(result.responses[1].markers.outputTruncated,true);
 assert.equal(result.responses[0].textBytes,Buffer.byteLength(update.rawOutput[0].text));assert.equal(JSON.stringify(result).includes('/secret'),false);
 for(let i=0;i<200;i++) audit.observe({...update,toolCallId:`many-${i}`});
 assert.equal(audit.snapshot().count,128);assert.equal(audit.snapshot().dropped,74);
});

test('inline context flag is optional, order independent and rejects duplicate or assigned values',()=>{
 assert.deepEqual(parseLimits(['--inline-context','--reserve-usd','1.6','--sdk-budget-usd','1.2']),{reserveUsd:1.6,sdkBudgetUsd:1.2,inlineContext:true});
 assert.equal(parseLimits(['--sdk-budget-usd','1.2','--inline-context','--reserve-usd','1.6']).inlineContext,true);
 for(const flags of [['--inline-context','--inline-context'],['--inline-context','true'],['--inline-context=true']]) assert.throws(()=>parseLimits(flags));
});
test('inline prompt embeds exactly the full compact provider packet and instructs emit only',()=>{
 const local={...packet,labels:['essential','supporting','incidental','uncertain'],humanRubric:'secret rubric',sourceFiles:[{path:'f.js',text:'UNIQUE_COMPLETE_SOURCE'}],candidates:packet.candidates.map(c=>({...c,source:'REPEATED_CALLER_SOURCE',calls:[]}))};
 const prompt=buildPrompt(local,{inlineContext:true});
 assert.equal(prompt.split('Provider packet JSON:\n')[1],JSON.stringify(providerPacket(local)));
 assert.equal(prompt.split('UNIQUE_COMPLETE_SOURCE').length-1,1);
 assert.equal(prompt.includes('REPEATED_CALLER_SOURCE'),false);assert.equal(prompt.includes('secret rubric'),false);
 assert.match(prompt,/Call only emit_view/);assert.match(prompt,/Do not call get_candidates or get_candidate/);
 assert.match(buildPrompt(local),/Read get_candidates once/);assert.equal(buildPrompt(local).includes('UNIQUE_COMPLETE_SOURCE'),false);
});
