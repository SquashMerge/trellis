# MCP read-only pilot: first-release wire contract

**Status: normative read-only MCP wire contract.** [Decision 0018](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md) records the owner’s #211 direction: a tenth tool, `trellis_search_symbols`, and `toolVersion:2`; [#528](https://github.com/SquashMerge/trellis/issues/528) moves it to `toolVersion:3` (see [Amendment (#528)](#amendment-528-imported-evidence-and-agent-sized-responses)). The owner approved activation with local voyage-4-nano and fixed weighted-RRF constants (20, 200, 1, 4), without an extra lexical restriction (2026-10-26 item-8 ruling). The [2026-10-28 #389 ruling](https://github.com/SquashMerge/trellis/issues/389#issuecomment-6482754535) adds exact-name admission and ordering, production-first lexical ranking, and the optional stored-role filter below; the constants are unchanged. [Decision 0012](semantic-evidence/decisions/0012-incoming-calls-syntax-tier.md) adds the sixth syntax-tier incoming tool; [#129 architecture rules](rules.md) adds three read-only tools and one no-argument proposal prompt. The original six tools retain their meanings; #593 adds the explicit stable-key selector alternatives and candidate-only branch below. [Decision 0007](semantic-evidence/decisions/0007-user-daemon-and-workspace-selection.md) makes `trellis mcp` a thin stdio client of the per-user daemon, with explicit checkout selection (any verified local Git checkout since [#839](#amendment-839-workspace-selects-any-local-git-checkout)); #107 amends the schemas below. [Decision 0008](semantic-evidence/decisions/0008-daemon-sole-writer.md) makes that daemon the only index and queue writer; [Decision 0009](semantic-evidence/decisions/0009-checkout-worker.md) assigns each active checkout one worker, pipelines relay calls, starts deadlines at protocol acceptance and sets the capacity and pre-resolution failure shape below. This is the agent-facing projection of [#22 semantic evidence](semantic-evidence/contract-v1.md) and [#23 local topology](local-topology.md). #22 owns evidence meaning, identity, validity, coverage and provenance; #23 owns coherent snapshot eligibility. This document owns only the wire projection. The configured client launches `trellis mcp` for one workspace selected by [#23 discovery](local-topology.md#workspace-discovery). The OS user, not an MCP principal or grant, is the boundary. No network listener, authentication role, ACP permission or tool annotation enforces access. Configuring an agent can disclose indexed checkout text to that agent/provider; repository text is untrusted data.

All code evidence in the twelve tools and proposal prompt comes from cached index state, never live source files. The original six tools retain their cached-only behavior. The only current-file exception is a bounded, once-per-request read of the fixed root `.trellis/rules.toml` as configuration by rules check/explain and prompt get. Its `{path,source:"checkout-config",contentHash}` descriptor is separate from the code pin; `evidenceBasis` does not identify those rules bytes. References alone do not read configuration. An admitted evidence read keeps one selected snapshot through response finish; describe can instead report not-ready status with a null basis. They never request indexing, start a producer/build, run repository code, open a terminal, invoke a provider or make a durable write. Search alone may embed the caller’s query using a local, pinned, in-process Candle model within the existing call deadline. This is not repository execution, a hosted-provider call, an indexing request or a durable write. Search reads but never fills the vector cache; the daemon’s separate background embedding owner fills it. No configured/verified/ready model, or no matching resident matrix under the owner’s memory policy, permits lexical-only search with `searchIndex.state:"unavailable"`. The daemon checkout worker alone owns mandatory startup reconciliation (H), watcher capture, publication and FIFO work; `catchingUp` reports pending work without waiving validated-head or root checks. No storage table, class or implementation mechanism is prescribed. The catalog order is `trellis_workspace_describe`, `trellis_find_symbols`, `trellis_search_symbols`, `trellis_declaration`, `trellis_outgoing_calls`, `trellis_incoming_calls`, `trellis_read_source`, `trellis_call_graph`, `trellis_references`, `trellis_rules_check`, `trellis_rules_explain`. Declaration, outgoing calls and incoming calls are separate closed tools, not views of an inspect tool; `trellis_inspect` is not recognized. Source uses the selected stored raw BLOB, verifies its path/version and whole-file hash/length before slicing, then strictly decodes only the requested LF-delimited range. Other selected readers, including public `source_at`, trust the published native, graph and class rows without re-attesting them against source (owner ruling 2026-10-23, #261). Source reads are not a disk fallback.

## Protocol, negotiation and transport

Normative modern revision: [official MCP 2026-07-28, commit `0cb6c6a31768cbb16129b35e6b569a31fecfe1b6`](https://github.com/modelcontextprotocol/specification/blob/0cb6c6a31768cbb16129b35e6b569a31fecfe1b6/docs/specification/2026-07-28/basic/index.mdx#L322-L409); see [discovery](https://github.com/modelcontextprotocol/specification/blob/0cb6c6a31768cbb16129b35e6b569a31fecfe1b6/docs/specification/2026-07-28/server/discover.mdx#L7-L59), [schema](https://github.com/modelcontextprotocol/specification/blob/0cb6c6a31768cbb16129b35e6b569a31fecfe1b6/schema/2026-07-28/schema.ts#L35-L110) and [stdio](https://github.com/modelcontextprotocol/specification/blob/0cb6c6a31768cbb16129b35e6b569a31fecfe1b6/docs/specification/2026-07-28/basic/transports/stdio.mdx#L7-L85). Support **exactly one** legacy revision, `2025-11-25`; no general older-SDK guarantee.

Use one newline-delimited UTF-8 JSON-RPC 2.0 message per line, **not** Content-Length framing. Stdout carries protocol messages only, stderr diagnostics only. Reject batches. IDs are non-null strings or safe integers; a string ID is admitted only when its decoded value, reserialized as one canonical compact JSON string token including the surrounding quotes, is at most 256 UTF-8 bytes. To form that token, wrap the decoded value in `"`; escape `"` as `\"` and backslash as `\\`; escape U+0008, U+0009, U+000A, U+000C and U+000D as `\b`, `\t`, `\n`, `\f` and `\r`, respectively; escape other U+0000–U+001F scalars as lowercase six-byte `\u00xx`; emit every other Unicode scalar directly as UTF-8, with no normalization or optional escaping. Measure the reserialized token, not the incoming escape spelling or Unicode scalar count. An oversized string ID receives a bounded JSON-RPC `-32600` response with `id:null` before connection-mode selection or pending-request registration; do not echo the ID. Numeric IDs retain their existing rule, and the full-line request cap still applies. An outstanding ID cannot be reused. EOF cancels outstanding work and exits. Do not promise detecting client `kill -9` while another process keeps stdin open. A valid `notifications/cancelled` has `{requestId,reason?:string}`; suppress every further response for that outstanding request, even a tool error. Ignore unknown/completed IDs and unknown notifications without responses. Cancellation overrides an unsent result. **Decision point (owner ruling 2026-10-11, #127):** a relay that holds a complete result settles every input frame the client finished writing before it decides, so any such cancel suppresses that result. A cancel written after the decision is concurrent with the result: the result is written once and the late cancel produces no output, as MCP clients already ignore responses to requests they cancelled. The settling wait counts only input written before the decision, so later input cannot delay a result; if it cannot settle within a bounded time, answer `outcome_unknown` instead of the result. Under Decision 0009, the relay pipelines calls, so the decision point applies **per client ID**. It covers every decided outcome, including the relay's own `deadline_exceeded`, and for a call its settling wait ends by that call's deadline. On the daemon connection, the relay uses its own monotonic IDs, so a late daemon reply never matches a client ID reused after its outcome was written. Cancellation is forwarded to the daemon instead of closing the connection. ("Mandatory H" below is Decision 0009's startup reconciliation.)

Modern mode has **no** initialize/initialized exchange. Each request has `params._meta` with literal keys `"io.modelcontextprotocol/protocolVersion":"2026-07-28"` and `"io.modelcontextprotocol/clientCapabilities":{}` (or valid capabilities); optional `"io.modelcontextprotocol/clientInfo"` follows MCP `Implementation`. These are single namespaced keys, not dotted nested objects. Notifications need no request metadata. `server/discover` takes only this protocol `_meta` and returns exactly `{resultType:"complete",ttlMs:0,cacheScope:"private",supportedVersions:["2026-07-28","2025-11-25"],capabilities:{tools:{},prompts:{}},_meta:{"io.modelcontextprotocol/serverInfo":{name:"trellis",version:<build-version>}}}`. The modern server identity is in result `_meta`, **not** top-level `serverInfo`; legacy `initialize` below uses its separate top-level field. Omit instructions; modern discovery and list results use required `ttlMs:0,cacheScope:"private"` caching hints, so a client may re-fetch instead of sharing any workspace-specific result. Advertise tools and prompts only: no subscriptions, sampling, elicitation, resources, tasks or client roots; empty client capabilities suffice.

In modern mode, `tools/list` takes only protocol metadata and returns `{resultType:"complete",ttlMs:0,cacheScope:"private",tools:[<twelve catalog entries>]}`; after legacy initialization it takes no parameters and returns `{tools:[<twelve catalog entries>]}`. Both list twelve entries in catalog order without pagination or next cursor; supplied cursor is a protocol parameter error. Each entry has its closed input schema below and an output schema that is the success/error envelope union. Each advertises `readOnlyHint:true`, `destructiveHint:false`, `idempotentHint:true`, `openWorldHint:false`. These are hints, not permissions or a promise that repeat reads have the same basis. `tools/call` takes `{name,arguments,_meta}` in modern mode and `{name,arguments}` in legacy mode. Both success and failure tool results contain `{resultType:"complete",isError:<boolean>,structuredContent:<full envelope>,content:[{type:"text",text:<summary>}]}` in modern mode; legacy results omit only `resultType`. `structuredContent` alone conforms to the tool’s closed output schema. The text is a deterministic UTF-8 summary of at most 512 bytes, **not** a JSON copy. Success uses `<tool>: <count> <items>[, truncated: <reason>]; full result in structuredContent.`; failure uses `<tool> failed: <code>: <message>`. For the original six tools, counts are one workspace for describe, the emitted number of symbols for find, one declaration for declaration, the emitted number of calls for outgoing and incoming, and the selected number of lines for read_source. Thus the item words are `workspace`, `symbols`, `declaration`, `calls`, and `lines`, respectively. Call graph counts the emitted `edges`. Search counts emitted `hits` and uses the item word `hits` in that summary template. The optional truncation clause appears only when `truncated:true`, and uses the exact `truncationReason`. Keep the full envelope, its evidence and all omission counts in `structuredContent`. Count the complete result, including both structured content and summary text plus LF, toward the 65536-byte frame cap.

Legacy mode starts with MCP `initialize` carrying `protocolVersion`, `capabilities`, `clientInfo`; respond with `protocolVersion:"2025-11-25"`, `capabilities:{tools:{},prompts:{}}`, `serverInfo:{name:"trellis",version:<build-version>}`. A different requested revision receives this supported revision per legacy negotiation; an incompatible client must disconnect. Wait for `notifications/initialized` before tools or prompt list/call/get; then use negotiated connection state, not modern required metadata. Repeated initialize and premature tool requests fail `-32600`. Select connection mode on the first valid modern request or initialize; reject switching modes. Cancellation remains available during work.

| Protocol condition | JSON-RPC error |
| --- | --- |
| Malformed JSON or UTF-8 | `-32700` |
| Invalid JSON-RPC shape, batches, oversized input, lifecycle violation | `-32600` |
| Unknown method | `-32601` |
| Invalid protocol parameters, missing modern metadata, supplied list cursor, unknown tool | `-32602` |
| Internal protocol failure or incompatible MCP relay/daemon identity | `-32603` |
| Valid non-tool stdio request refused because the connection already has 16 outstanding non-call requests (Decision 0009) | `-32000` Server error; original ID and bounded `error.data.code:"too_many_requests"` |
| Unsupported modern revision | `-32022` |

Before forwarding MCP traffic, the relay and daemon compare a private protocol
identity on every socket attachment, including replacement daemons. The identity
binds the complete tool catalog and relay protocol revision; it is independent of
build diagnostics and index identity. Missing or different identities refuse the
attachment before workspace attachment or tool execution. The relay returns a
JSON-RPC error (not a tool result) with `error.data.code:"mcp_restart_required"`,
`error.data.retryable:false`, and an instruction to restart the MCP server. That
relay stops reconnecting after a mismatch. Tool output schemas are unchanged.

For `-32022`, include required JSON-RPC `error.data:{supported:["2026-07-28","2025-11-25"],requested:<requested-version>}` so the client can negotiate; the requested value is the unsupported revision received. `-32021` is reserved for genuinely required missing client capability (none here); `-32020` HeaderMismatch does not apply to stdio. Bad **application** arguments to a known tool instead return the typed error envelope. Oversized wire input is discarded through its line delimiter and receives bounded `-32600`; malformed parsing has `-32700` precedence when no valid request was obtained.

### Compatibility decision (W7)

| Recorded evidence | Honest conclusion |
| --- | --- |
| Pinned [MCP TypeScript SDK 1.30.0](../tools/selection/package.json), [supported revisions](https://github.com/modelcontextprotocol/typescript-sdk/blob/1.30.0/src/types.ts#L4-L6), [client initialization](https://github.com/modelcontextprotocol/typescript-sdk/blob/1.30.0/src/client/index.ts#L464-L505) and [offline initialize/list/call smoke](../tools/selection/acp-smoke.test.mjs) | Supports `2025-11-25`; justifies the legacy target, **not** tested Trellis interoperability. |
| [Claude Code 2.1.266, Claude ACP 0.79.0 and underlying Claude Agent SDK 0.3.274](research/selection/ACP-SETUP.md); [successful Claude ACP experiment](research/selection/SMOKE-RESULTS.md) | Initialize-era MCP and an ACP experiment are recorded. ACP wire version 1 is **not** an MCP revision. Exact MCP negotiation was not recorded; legacy mode is a target for #17 fixture verification, not certified Claude/modern support. |
| Codex CLI 0.150.1, Codex ACP 1.12.0 and legacy ACP 0.16.0 (historical evaluation of the retired answer adapter) | Considered, not certified. CLI inference smoke proves neither ACP/MCP interoperability nor the evidence-only boundary. |

## Shared schema and limits

Every application object is closed: unknown fields fail. All fields are required unless marked `?` optional; a nullable field is **present**, with null allowed. Explicit null is invalid for an optional non-null input. JSON `UInt` is `[0,2^53-1]`; strings contain valid Unicode scalars; byte limits count UTF-8, never characters. Import #22's [Text, Language, Kind, Range, SyntaxId, OccurrenceId](semantic-evidence/contract-v1.md#conventions-and-shared-types), [DocumentKey, Coverage, Provenance, SemanticBasis](semantic-evidence/contract-v1.md#identity-and-coverage), [freshness](semantic-evidence/contract-v1.md#basis-and-freshness), [Declaration](semantic-evidence/contract-v1.md#records-and-bindings), [CallBinding](semantic-evidence/contract-v1.md#dispatch), [Hash](semantic-evidence/contract-v1.md#conventions-and-shared-types) and [canonical IDs](semantic-evidence/contract-v1.md#stable-identifiers), and [warnings-v1](semantic-evidence/contract-v1.md#warning-completeness-warnings-v1) without redefining their nested fields or invariants. `Range` is zero-based, half-open, UTF-8 scalar-aligned. A symbol ID is canonical `sid:v1:` plus 32 lowercase hex digits; examples below use illustrative valid-shaped IDs, **not** #22 identity vectors.

| Wire type | Exact shape and rule |
| --- | --- |
| `Basis` | `{indexGeneration:string,indexRevision:UInt}`; generation is fresh on creation/rebuild, non-nil lowercase canonical UUID; revision advances per successful publication. Not #22 revision ID: the snapshot unambiguously maps to each evidence record's source-set/revision identity; inconsistent mapping fails `store_unavailable`. |
| `Evidence` | `{evidenceTier:"syntax"\|"semantic",semanticBasis:SemanticBasis\|null,freshness:"fresh"\|"possiblyStale"\|"stale"\|"unavailable",staleBecause:string[]}`. Successful first-release value is **only** `syntax,null,fresh,[]`, relative to eligible captured snapshot, not instantaneous disk. No unavailable placeholder or promised schema-compatible automatic semantic expansion. |
| `DocumentRef` | `{ref:UInt,document:DocumentKey,revisionId:Text,contentHash:Hash,provenanceId:Text}`; the per-response table entry for one emitted document version. `ref` is contiguous from zero and is the value of an emitted item’s `doc`. |
| `Lines` | `{start:UInt,end:UInt}\|null`; non-null bounds are 1-based inclusive LF-delimited source lines, with `1<=start<=end`. Null is reserved for a zero-length range in an empty cached file. |
| `Symbol` | `{symbolId:SyntaxId,name:Text\|null,displayKey:Text,stableLookupKey:Text\|null,kind:Kind,role:"test"\|"production",doc:UInt,range:Range,nameRange:Range\|null,lines:Lines,...Evidence}`; `displayKey` is the selected declaration’s qualified presentation label defined below, never an ID or lookup proof. `doc` supplies the selected declaration’s document version, whole-file content hash and native proof. Preserve #22 name/nullability and identity. `role` is required, copied from the selected measured declaration, and is not #22 evidence `Role`. |
| `NativeCandidate` | `{syntaxId:SyntaxId,name:Text,doc:UInt,range:Range}`; fields project one verified fresh native `Declaration` in the eligible response snapshot. Its `doc` entry’s `provenanceId` equals that declaration’s verified native proof, not a borrowed match proof. `name` and its language-decoded lookup key are non-null; this is a lexical hint, **not** a `Target` or binding. Candidates have no `lines`. |
| `NativeCandidates` | `{items:NativeCandidate[],omitted:UInt\|null,complete:false}`; required on every outgoing `CallItem`, at most eight items. An integer counts eligible matches not shown for **that emitted call**: verified hints dropped for response bytes plus distinct eligible matches left unexamined once eight hints passed (#170); null means optional hint matching/count did not finish within its separate work budget and requires `items:[]`. `complete:false` is invariant. Excluded calls and unknown matches are never counted. |
| `Warning` | `{code:"coverageIncomplete"\|"staleEvidence"\|"staleTarget"\|"bindingAmbiguous"\|"syntaxOnly",message:Text,provenanceId:Text\|null}`. |
| `Limits` | `{requestBytes:16384,responseBytes:65536,sourceBytes:16384,sourceLines:200,defaultLimit:20,callDefaultLimit:6,maxLimit:50,pageBytes:24576,calleeTextBytes:1024,hintWorkMs:1000,deadlineMs:5000,concurrency:4}`. `defaultLimit` is find's; `callDefaultLimit` is outgoing and incoming calls' (#528); `pageBytes` is the architecture page target (#528). |

For W2–W4, incoming calls and search, `documents:DocumentRef[]` is a **wire projection**, not an amendment to #22 `Declaration` or #22 proof semantics. Each `doc` resolves to exactly one entry with matching `ref`; no entry is unused. Build the table from exactly the distinct document versions used by **emitted** symbol, declaration, call and shown candidate `doc` fields, not the searched scope, an omitted prefix, or a selected owner present only in outgoing/incoming coverage and provenance. An empty emitted item set has `documents:[]` even if owner closure remains. Identity is the full `DocumentKey` (`language`, `sourceSetId`, `path`) plus `revisionId`, never path alone. A different content hash or provenance proof for the same identity is `store_unavailable`, not an arbitrary ref choice. Sort identities by `(language in #22 order: java, rust, python, javascript, typescript; sourceSetId bytes; path bytes; revisionId bytes)` and assign `ref` and matching `doc` as consecutive zero-based integers. For tools other than search, equal snapshot and request yield byte-identical table, refs and serialization. For search, the same admitted index snapshot and emitted document-version set yield byte-identical document table, refs and table serialization; the emitted set itself may change between calls at the same evidence basis as the vector cache changes. This table replaces only the named duplicate item fields: Symbol's `document,contentHash,provenanceId`; declaration data's outer `contentHash` and nested Declaration's `document,revisionId,provenanceId`; CallItem's `document,revisionId,contentHash,provenanceId`; and NativeCandidate's `document,revisionId,provenanceId`. Do **not** compact `Provenance.document` or coverage tuples: keep their full #22 shapes, sorting and actual proof. Every item range and evidence remains unchanged.

`lines` describes an item’s own `range` in its `doc` entry's selected cached raw BLOB, without opening a live file or decoding unrelated source. For a nonempty byte range use the LF-delimited line containing `range.start` and the line containing `range.end-1`; for a zero-length range use the line at `range.start`. At EOF of a nonempty BLOB, use its last selectable line, even if the final LF ends the file; do not invent a post-LF line. An empty BLOB's zero-length range has `lines:null`. No UTF-8 decoding is needed to locate LF boundaries. A same-`evidenceBasis` and same-`contentHash` W5 read of `path` from the referenced `DocumentRef` using the returned inclusive `lines` has a `byteRange` containing the item’s range **if** the selected slice has at most 200 lines, at most 16384 raw bytes and is valid UTF-8. Otherwise retain non-null `lines`, and W5's existing `range_too_large` or `unsupported_encoding` applies; multiple bounded reads may cover a long range collectively, but one over-cap read is not claimed to contain it. Empty-BLOB null is exempt. W5 path lookup and its guards must still select and verify that same document version or return its existing typed failure; a path alone is not license to substitute a different source set.

Qualified `displayKey` for a measured declaration `d` is `d.document.path + "#" + join(".", ancestorSegments + [ownSegment])`: each outer-to-inner ancestor segment is its measured `Key.name` or, when null, `Key.kind + "@" + decimal(Key.ordinal)`; the own segment is `d.name` or, when null, `d.kind + "@" + decimal(d.range.start)`. Preserve path/name bytes with no normalization or inferred scope. For example `src/a/Invoice.java#Invoice.total` is a readable label; unnamed declarations use their measured fallback. The label is always `Text` when required selected declaration evidence is valid, but it is **not unique** (Java overloads may share it), is never parsed as an ID, and never enters requests, hashes, lookup, ranking, ordering, joins, resolution or completeness. An unavailable/malformed required selected declaration is `store_unavailable`, not a null or fabricated label.

Workspace selection applies to the twelve tool calls; the no-argument proposal prompt uses the launch checkout. Discovery, tools/list and prompts/list remain workspace-independent. All twelve flat input schemas retain `additionalProperties:false`; the declaration, outgoing-call and incoming-call inputs are separate closed objects, with optional `workspace` alongside their own fields. There is no inspect union or `view` discriminator. The MCP client resolves its launch checkout from `--workspace` or the nearest Git checkout from its launch directory. Omitted `workspace` selects that launch checkout. An explicit `workspace` is a verified absolute directory path (at most 4096 UTF-8 bytes) that resolves to a Git checkout root, without silently following a symlink to a different selection. Any verified local Git checkout may be selected: a linked worktree of the launch repository, a `git clone --local` copy or an unrelated repository; its Git common directory need not match the launch checkout's (#839). The daemon checks the selected root's identity; nearby paths or equal markers do not establish identity. A path that is not a checkout root or cannot be verified is refused, never answered from the launch checkout instead. Selection attaches the checkout through the MCP session's lifetime. Each admitted evidence result uses one checkout and one coherent, committed and validated snapshot; no evidence from different checkouts mixes. Describe may instead report no ready head. A valid prior head remains readable during startup reconciliation, watcher work or pending FIFO requests with `catchingUp` sampled at read admission; it does not promise current-disk freshness or authorize an unvalidated head. With no valid head, evidence reads return `index_not_ready`. Root identity is checked before admission and at the daemon send decision.

Success envelope for a **resolved** checkout: `{schemaVersion:1,requestId,workspace:string,catchingUp:boolean,evidenceBasis:Basis|null,data,warnings:Warning[],partial:boolean,truncated:boolean,truncationReason:null|"response_bytes"|"limit"|"callee_text"}`. `workspace` is the answering checkout's verified absolute root, never its label or the caller's unverified input. `catchingUp` is sampled with that checkout's read admission. `requestId` equals the outer JSON-RPC ID. Only describe may succeed with a null basis. There is no `ok` wrapper. Resolved failure: `{schemaVersion:1,requestId,workspace:string,catchingUp:boolean,error:{code,message,retryable,currentBasis:Basis|null,currentContentHash:Hash|null}}`, without success fields or evidence, and `isError:true`. A non-null current basis is safely verified eligible; otherwise null. A non-null current hash occurs **only** for a hash mismatch at a found cached path, never from disk. `checkout_capacity` is a resolved-checkout error when a verified selection cannot acquire one of the 64 active-checkout slots.

Selection failure has no answering checkout: `{schemaVersion:1,requestId,error:{code:"workspace_selection_failed",message,retryable,reason,attemptedWorkspace,currentBasis:null,currentContentHash:null}}`. Reasons are `not_absolute`, `not_checkout`, `identity_changed`, `unavailable`; only `unavailable` is retryable. An omitted selection whose launch checkout cannot be verified also fails without answering attribution; `attemptedWorkspace` describes its attempted launch path. A non-string `workspace` on a known tool is instead **application** `invalid_request` with `{schemaVersion:1,requestId,error:{code:"invalid_request",message,retryable:false,attemptedWorkspace,currentBasis:null,currentContentHash:null}}`, never JSON-RPC `invalid_params`; neither failure has `workspace` or `catchingUp`. `attemptedWorkspace` is closed and bounded: string input becomes `{kind:"path",value:string,utf8Bytes:UInt,truncated:boolean}`, where `value` is the first at most 256 UTF-8 bytes cut at a Unicode scalar boundary, `utf8Bytes` is the exact input byte count, and `truncated` reports clipping. Short absolute paths are retained exactly. A string longer than 4096 UTF-8 bytes fails `invalid_request` with that tag rather than selecting its truncated prefix. Non-string input becomes `{kind:"invalid_type",jsonType:"null"|"boolean"|"number"|"array"|"object"}`; never serialize a raw array/object or stringify it ambiguously. These are the only additions to the closed failure `error` object: `reason` occurs only for `workspace_selection_failed`, and `attemptedWorkspace` occurs on selection failure and invalid-type/oversize `workspace` failure. Other bad application fields retain their existing error and precedence; they do not claim a checkout unless one has already been resolved. A known tool's malformed application arguments still use a tool envelope; protocol-only errors remain JSON-RPC errors. `daemon_unavailable` is retryable; `outcome_unknown` is not automatically retryable and requires request/job status inspection rather than replaying an ambiguous mutation.

Find, search, declaration, outgoing-call and incoming-call data objects contain `coverage:Coverage[]` and `provenance:Provenance[]`. Declaration coverage contains the selected declaration’s relevant native tuples; outgoing-call coverage contains those of the selected declaration, emitted measured calls and shown `nativeCandidates` declarations. Find coverage contains the native tuples of the documents of the **emitted** symbols only, keyed by full `DocumentKey`, revision and producer, deduplicated (amended by #156, owner-ratified 2026-10-16); the searched scope is reported as aggregate `scopeCoverage` counts (W2). Search uses exactly this emitted-hit closure and searched-scope rule; its relevance relation has no provenance. Provenance is the complete closure for returned evidence, including returned symbols, the selected declaration, emitted calls and shown native-candidate declarations; do not fabricate it for the name-match relation, failed extraction or negative search. Sort coverage by `(producerId,language,sourceSetId,documentPath,revisionId)` with #22 enum/byte order and provenance by ID. Missing required rows, corrupt digests or mixed revisions are hard failures (`store_unavailable`), never best-effort warnings. Describe and read_source have neither array.

## Amendment (#528): imported evidence and agent-sized responses

Owner-directed amendment from the 2026-10-31 live review (gaps 12–13 and the MCP dogfood re-run). It is a pre-release response-contract change without compatibility: `toolVersion` is `3`. Where a passage elsewhere in this document says otherwise, this section wins.

- **Language status.** `LanguageStatus` (describe `languages`, find/search/incoming `scopeCoverage`) is `{language,evidenceTier:"syntax",coverage,semantic}`. `evidenceTier` and `coverage` keep describing the **native syntax** tuples, as answers built from native declarations do. `semantic` is `{available:boolean,producerId:Text|null,freshness:"fresh"|"possiblyStale"|"stale"|"unavailable",coverage:{selected,complete,partial,failed,unselected},note:Text|null}`: the language's selected imported producer under the per-language default (#483) at the admitted head (an explicit `semanticProducerId` does not change it), its tuple counts (same buckets as native) and the worst freshness of the proofs of its selected complete or partial tuples. Publication gives every proof of one tuple the same freshness, so one proof per tuple is read. After an edit and before a re-import, carried-forward proofs are `possiblyStale` and `available` is false even though call items still carry those bindings with `freshness:"possiblyStale"`. `available` is true iff a producer is selected, its freshness is `fresh` and at least one tuple is selected complete or partial. With no producer, `producerId:null`, `freshness:"unavailable"`, zero counts and `note` gives the #483 reason. Partial coverage of either tier stays visible; describe's `partial` is also true for a selected partial/failed semantic tuple. These are availability facts, not a claim that the answer used semantic evidence: each item keeps its own evidence tier (find and search hits and declarations are native records, `syntax`; call items are `semantic` only when they carry a binding). **Partial reasons (#520).** In describe only, `semantic` also has `partialReasons`: `null` when the language has no producer or no partial semantic tuple, else `{nativeGaps,nativeGapsOnly,capped,omitted,capturedDiagnostics,unrecorded,unsupportedOccurrences:{macroInvocation,attributeItem,macroDefinition,other},summary:Text}`, the import-time [coverage reasons](semantic-evidence/contract-v1.md#coverage-reasons-520) of those tuples with the same counts and one-line `summary` as `trellis status` (for example `partial: 309 · native macro/attribute gaps only`, or `failed: 106 · not in producer output; partial: 252 · native macro/attribute gaps only` when the language also has failed tuples; the failed part comes first and is left out when none failed). `scopeCoverage` does not carry it.
- **`syntaxOnly`.** For find, search and declaration it says the answer is syntax-derived and names the languages without available semantic evidence: `This answer is syntax-derived; no fresh semantic evidence is selected for <languages>` (#22 order). The languages are those of the emitted items (the declaration's own; with no hits, the searched languages with selected native tuples). It is omitted when every such language has available semantic evidence. With nothing to attribute it is `This answer is syntax-derived`. Read source keeps that plain warning. Outgoing and incoming calls keep the #467 rule.
- **Semantic basis table.** Outgoing and incoming data add `semanticBases:SemanticBasisEntry[]`, each distinct `SemanticBasis` of the emitted items and proofs once, as `{ref:UInt,...SemanticBasis}` with `ref` contiguous from zero in canonical-serialization order. A call item's `semanticBasis` is replaced by `semanticBasisRef:UInt|null`, and a provenance row's `basis` by `basisRef:UInt|null` (null for native rows). Provenance remains the deduplicated closure by ID; documents and coverage rows are unchanged.
- **Compact mode.** Outgoing and incoming accept `compact?:boolean` (default false; any non-boolean is `invalid_request`). When true, data omits `coverage`, `provenance` and `semanticBases`, and each item omits `binding` and `semanticBasisRef`. For the same emitted prefix, items, document refs, dispositions, targets, boundaries, evidence tiers, warnings, `partial` and `scopeCoverage` are those of the full answer; because the response-byte fit uses the smaller compact frame, more items may fit, so `truncationReason` can be `limit` where the full answer would be `response_bytes`. A warning's `provenanceId` names a proof that compact does not include; request the full answer to resolve it. Compact never hides incompleteness.
- **Default pages.** Outgoing and incoming `limit` defaults to 6 (still 1–50). Six semantic callers of `index_workspace` with full proofs measured 23.5 KB on this repository; 20 compact callers measured 18 KB. Optional outgoing `nativeCandidates` hints fill only while the whole frame stays under the same 24,576-byte `pageBytes` target (required calls still fit the 65,536-byte cap); hints left out are counted in that call's `omitted` as before. Architecture paging (#448) fills a page to the 24,576-byte `pageBytes` target instead of the 65,536-byte cap; a lone item larger than the target still gets its own page under the cap, and only an item over the cap fails `response_too_large`.
- **Capacity messages.** `too_many_requests` names its limit: `Too many concurrent tool calls on this connection (limit 4); wait for one to finish`, or `Too many concurrent tool calls in the daemon (limit 16); retry shortly`; the non-call `-32000` refusal says `Too many outstanding requests on this connection (limit 16)`. The summary text carries the same message.
- **Descriptions.** Every catalog entry has an agent-facing `description` naming its purpose, defaults and compact mode.

## Amendment (#593): stable declaration lookup keys

The [2026-11-01 owner ruling](https://github.com/SquashMerge/trellis/issues/593#issuecomment-6563834529) adds readable lookup keys without changing `sid:v1`, `displayKey`, native `Declaration.lookupKey`, semantic identity or graph expansion. `toolVersion` is **4** (previously 3). This section wins over older selector and version descriptions below. This first slice covers MCP only, not CLI or browser selectors.

Every find `Symbol` and search hit includes required `stableLookupKey:Text|null`. Declaration data, outgoing-call data, incoming-call data and call-graph data include the same required top-level field for the selected root. Null means a stable named key cannot be derived; it is not an alternate identity. Native `Declaration.lookupKey` remains the language-decoded leaf-name key used by name matching. `displayKey` remains output-only presentation.

`trellis_declaration`, `trellis_outgoing_calls`, `trellis_incoming_calls` and `trellis_call_graph` accept exactly one of `symbolId:SyntaxId` or `lookupKey:string`. `trellis_find_symbols` accepts exactly one of `query:string` or `lookupKey:string`. A lookup key is nonempty and has no new per-field length cap; the existing 16,384-byte complete request cap applies. Both selectors, neither selector, null or a wrong type are `invalid_request`. Existing query bounds, SID validation, other fields and defaults remain unchanged. `limit` still validates where accepted but does not limit exact-key enumeration. Search input is unchanged.

Key spelling is `path::outer::name`: preserve the stored path and outer-to-inner named declaration segments, joined with `::`, without escaping or normalization. Omit the synthetic file module. An unnamed declaration or unnamed non-implementation container makes the key null, including its descendants. For Rust implementation containers use the written self-type path from the bounded AST, preserving its named path segments while stripping generic arguments and reference qualifiers; trait names and implementation specialization do not distinguish keys. For example both trait and inherent implementations may emit `src/store.rs::Store::read`. Duplicate keys across implementations, overloads or macro-generated declarations are intentional. Allowlisted macro implementation syntax follows the same naming rule. A stored path may itself contain `::`; resolution considers every selected document path prefix rather than splitting at the first separator.

Derivation uses only selected cached source and measured declaration relationships. Existing Files parsing bounds remain 2 MiB and 250 ms, plus syntax traversal depth 128 and one million visits; this amendment adds no bound. Every required implementation syntax range must join its measured declaration, including supported macros. A parse-budget failure is typed `deadline_exceeded`, never a skipped declaration or false unique match.

Keys are exact, case-sensitive emitted strings, not fuzzy matches, display labels or strings to normalize. Resolution compares the entire key, including its path; separators are never parsed from caller input. It reads the same admitted cached snapshot as the requested operation. Expected-basis comparison precedes resolution. Missing SID requests never infer a key or fall back. A unique key delegates to the unchanged SID operation in that same snapshot. Zero matches return `not_found` for rooted tools; exact-key find returns its ordinary empty `symbols` success. These remain indexed-snapshot statements, not claims of universal extraction completeness.

More than one key match returns **only** the common alternative success data object `{lookupKey:Text,candidates:Symbol[],documents:DocumentRef[],coverage:Coverage[],scopeCoverage:LanguageStatus[],provenance:Provenance[]}`. No declaration/call/graph operation runs. Every candidate uses the ordinary Symbol summary, including exact SID, readable key, range, source document and evidence. Document/proof closure, scope coverage and warnings follow find. Candidate-only native provenance retains find's `basis:null` shape, including on outgoing/incoming tools; it does not use those tools' operation-only `basisRef`/`semanticBases` projection. There is no semantic resolution or binding warning for selector ambiguity. Success text counts `candidates` before reading operation-specific fields.

**All-candidate bounds qualification:** enumerate all exact matches before testing cardinality, without the ordinary 50-result limit. Return every candidate, never pick one from a prefix. Both candidate-only success and exact-key find are atomic under the existing 65,536-byte complete JSON-RPC response cap (including summary and LF). If the whole result cannot fit, return typed `range_too_large`, with no partial candidate list and no operation. Do not apply ordinary query-mode find prefix clipping. Existing coverage incompleteness can still make a complete candidate list `partial:true`; it is not list truncation. Parsing or projection failures must not turn an incomplete key inventory into false absence or uniqueness.

## Amendment (#594): user model configuration diagnostics

`toolVersion` is **5** (previously 4). This amendment supersedes older version descriptions, but does not change #593 lookup keys, selectors or ambiguity responses. Workspace describe adds optional `searchIndex.configuration:SearchConfiguration`. Its closed shape is:

```text
SearchConfiguration={configFile:string,modelsDir:string,modelName:string,
 modelRevision:string,modelPath:string|null,backend:"auto"|"cpu"|"metal",
 sources:{modelsDir:ConfigurationSource,model:ConfigurationSource,backend:ConfigurationSource}}
ConfigurationSource="default"|"config_file"|"environment"
```

All fields inside `configuration` and `sources` are required. Only `modelPath` is nullable: an empty development model-path override disables the model; a missing managed snapshot still reports its resolved path. A production daemon publishes the startup-resolved configuration in workspace describe and CLI status. A standalone registry or injected test model without configuration knowledge omits `configuration`, rather than inventing defaults or reporting null. `configuration.backend` is the requested preference; sibling `searchIndex.backend` remains the actually selected backend. Model name/revision describe configuration, not a verified, loaded or ready model, and do not replace query-specific `SearchIndexStatus`.

Configuration is read once at daemon startup from `~/.config/trellis/config.toml` on macOS and Linux, independently of XDG configuration variables. It is user configuration, not repository evidence or a per-request file read. Root `models_dir` (absolute) and `backend` are optional; optional `[model]` selects the reviewed `name`/`revision` pair. Defaults use the Trellis data root's `models` directory, the existing reviewed voyage-4-nano pin and automatic backend selection. Managed snapshots live at `<modelsDir>/<modelName>/<modelRevision>`. Field-wise development overrides `TRELLIS_SEARCH_MODEL` and `TRELLIS_SEARCH_BACKEND` take precedence over file values and defaults; they are not the normal installation path. `sources` records the winning source for the model directory, model path/selection and backend. Changes require daemon restart to affect this snapshot. Invalid, unsupported or unreadable configuration fails startup even when an environment override is present; defaults are not written to disk.

Ordinary startup, describe, status and search never download models. Installation is explicit and verifies the existing reviewed filename/hash pins before publishing the managed snapshot. It does not write configuration or restart the daemon; installing the default snapshot takes effect at the next manual daemon start. No hash, model/cache identity, evidence meaning, database schema, ranking constant or response cap changes here. Missing or failed model loads retain lexical search and the existing unavailable reasons.

## Amendment (#594): native coverage labels

`toolVersion` is **6** (previously 5). In every `LanguageStatus` row (describe's
`languages` and every `scopeCoverage`, including lookup candidates), rename
`evidenceTier` to `nativeCoverageTier`. Its value remains `"syntax"`: it describes
only the native tuple counts in sibling `coverage`, not the selected semantic
producer or the evidence used by an answer. `semantic` retains its separate
availability, freshness and coverage. Item-level `Evidence.evidenceTier`, bindings,
provenance and warning semantics are unchanged. This supersedes the #528 field
name above with no alias or backward compatibility. The web source-margin action
likewise says `measured declaration`, describing its entry point rather than the
sequence's call evidence.

## Amendment (#632): retained semantic re-import status

`toolVersion` is **7** (previously 6). Each closed `LanguageStatus.semantic`
object adds required `reimport:null|{state:"needsReimport",reason:Text}`. This
reports retained input that still needs successful validated publication after a
rebuild. A refusal or mismatch keeps its useful reason until the corresponding
producer successfully publishes through normal import. Queue acceptance and a
started attempt do not clear it. Multiple producers' outstanding reasons for one
language are joined in producer-ID order; one producer's success does not clear
another's outstanding input.

The overlay is independent of `available`, `producerId`, `freshness` and coverage.
Those still describe only selected published evidence. Retained bytes do not
select a producer or claim fresh proof. A language with no published producer
remains unavailable even when it has a re-import reason. Another producer's valid
published evidence may coexist with outstanding retained input. `null` means no
known outstanding retained input, not that evidence is fresh or a producer is
configured. The same selected snapshot supplies this field in describe
`languages`, operation `scopeCoverage` and lookup alternatives. Native coverage,
`syntaxOnly`, completeness and `partial` meanings are unchanged. MCP does not run
a producer or initiate import; a mismatch requires a current artifact to be
captured and imported through the existing explicit workflow.

## Amendment (#665): agent-sized call graphs and call lists

From the owner's rc.1 dogfood run (2026-11-03): `trellis_call_graph` with its
defaults filled the 65,536-byte response at about eight nodes, because every proof
repeated the same imported basis. `toolVersion` is **8** (previously 7). This
section wins over the call graph's earlier input and success-envelope text; the
contract-v1 graph records, traversal, warnings and byte-fit rules are unchanged.

- **Basis table.** Call graph data adds `semanticBases:SemanticBasisEntry[]` and
  each `result.provenance` row carries `basisRef:UInt|null` instead of `basis`,
  exactly as the #528 outgoing and incoming table does. Re-inlining the table
  gives back the contract `GraphResult`.
- **Compact graph.** `trellis_call_graph` accepts `compact?:boolean` (default
  false; a non-boolean is `invalid_request`). When true, data omits
  `semanticBases`, the result omits `coverage` and `provenance`, each edge
  replaces `binding` with `resolution` (the binding's resolution, or null without
  one), each node declaration keeps only `role`, `syntaxId`, `document`, `kind`,
  `name`, `lookupKey` and `range`, and each edge call keeps only `id`,
  `ownerSyntaxId`, `ordinal`, `range`, `calleeRange` and `spelling`. Nodes, edges,
  visits, boundaries, frontiers, `omitted`, `hints`, warnings, `partial` and
  `truncated` are those of the full answer for the same request; the byte fit uses
  the compact frame. On this repository's shape a compact answer is about a fifth
  of the full one.
- **Default limits.** The tool materializes its own defaults, sized to the
  response: `maxNodes` 8 and `maxCalls` 10, or 50 and 60 with `compact:true`.
  Eight nodes and ten semantic edges with proofs measured about 50 KB; fifty
  nodes and sixty compact edges about 51 KB. `depth` stays 2, and the valid ranges
  (1–150 nodes, 0–500 calls) are unchanged.
- **Omitted counts.** Data adds required `responseOmitted:{nodes:UInt,edges:UInt}`:
  the nodes and edge groups of the bounded answer that a response-byte reduction
  left out, zero otherwise. The summary text repeats them, for example
  `trellis_call_graph: 11 edges, truncated: response_bytes (110 nodes, 109 edges
  omitted; compact:true fits more); full result in structuredContent.`
- **External calls in compact outgoing calls.** `trellis_outgoing_calls` accepts
  `includeExternal?:boolean`, default true, or false with `compact:true`.
  When false, calls whose selected binding resolves to an external symbol (the
  call graph's published external calls, stale ones included) are left out before
  `limit` applies, and data adds `externalOmitted:UInt`, their count. Kept calls,
  warnings and completeness are computed as before over the kept calls.
- **Find ranking.** `trellis_find_symbols` query hits list callables, types and
  every kind other than fields, parameters and variables first, then those three;
  within each class the exact, prefix, substring, role, name and ID order is
  unchanged. Lookup-key resolution and search are unchanged.

## Amendment (#724): callers bound elsewhere and bounded graph spellings

From the v0.1.0 eight-task MCP dogfood run (2026-11-05). `toolVersion` is **9**
(previously 8).

- **Callers bound elsewhere (owner ruling 2026-11-06).** Callers whose fresh
  direct binding targets a different same-name declaration are excluded from
  incoming results and counted. This amends the #467 sentence "the binding
  annotates each found call and adds no callers" to "the binding annotates each
  found call, adds no callers, and removes callers proven bound elsewhere".
  - *Proven bound elsewhere* means the call's selected binding (the shared
    #466 reader, under the request's producer selection) has effective
    freshness `fresh`, `boundaryReason:"none"`, and a located `targetId` other
    than the selected `symbolId`. These callers stay listed exactly as before:
    a binding to the selected symbol, a `dispatch` boundary (dispatch is not
    proven direct), a stale or possibly stale binding, a binding without a
    located target, and a caller with no binding.
  - The exclusion runs while the ranked name matches are scanned, before
    `limit` and `has_more` are decided. Ranking is unchanged. The scan keeps
    the first `limit` callers that are not excluded and stops at the next one,
    which sets `truncationReason:"limit"`. A page therefore holds `limit`
    callers whenever that many remain, and an excluded caller never makes a
    page look truncated. Ranked rows are fetched one at a time, each after the
    call's existing cancellation and deadline check, and the scan stops at the
    first kept caller beyond `limit`; the query's own ranking before its first
    row is not interrupted by that check. When no producer is selected for the
    searched language, nothing can be excluded and the scan reads at most
    `limit + 1` rows, as before. The 65,536-byte response fit applies to the
    kept callers as before.
  - Data adds `boundElsewhereOmitted:UInt` when at least one caller is
    excluded. It counts the excluded callers among the ranked matches the scan
    read: all of them when the scan reached the end (no `limit` truncation),
    otherwise those ranked before the first kept caller beyond `limit`. It is
    exact when `truncated` is false and a lower bound of the total otherwise.
    The field is absent when nothing is excluded, and that response is
    unchanged.
  - `match` (basis `name`, `sameNameDeclarations`, limitations) is unchanged.
    Unbound callers can still call another same-name declaration. Exclusion
    adds no warning and does not by itself set `partial`. Outgoing calls and
    the call graph are unchanged.
- **Bounded graph spellings.** `trellis_call_graph` clips each edge call's
  `spelling` to the advertised `calleeTextBytes` (1024 UTF-8 bytes) on a scalar
  boundary, as outgoing calls clip `calleeText`, in full and compact form. A
  clipped call adds `spellingTruncated:true`; a spelling that fits is unchanged
  and carries no flag. Clipping is a projection step: the raw traversal (its
  nodes, edges, visits, boundaries, frontiers and `GraphResult.truncated`) is
  unchanged, and no `callee_text` truncation reason is added. The existing
  response byte fit applies to the clipped representation, so the emitted wire
  answer can differ: a spelling of 1025 ASCII bytes becomes 1024 bytes plus the
  25-byte flag (net +24), while a much longer one shrinks. Near the response cap
  the fit can therefore keep fewer or more edge groups, and can set or clear
  `truncationReason:"response_bytes"` and `responseOmitted`.

## Amendment (#725): rule evidence bases, status filter and instance pages

From the 2026-11-05 dogfood (#714): at `limit:50`, a `trellis_rules_check` page held
one instance. Each evidence row repeated its whole semantic basis, and `limit`
sampled evidence per instance rather than counting instances. `toolVersion` is
**10** (previously 9). This section wins over the #129 input table, the check/explain
`limit` sentence and the paging text below, and over the MCP paragraph of
[rules.md](rules.md#check-and-explain-data). Evaluation, verdicts, instance order,
counts and the CLI and browser shapes are unchanged.

- **Basis table.** Check and explain data add the required
  `semanticBases:SemanticBasisEntry[]`. Each rule evidence row carries
  `basisRef:UInt|null` instead of `basis`, as the #528 outgoing and incoming tables
  and the #665 call graph table do. This covers an instance's `evidence` and each
  constraint's `evidence`. Each distinct basis of the page's rows appears once as
  `{ref,...SemanticBasis}`, with `ref` contiguous from zero in canonical-serialization
  order. A null `basis` becomes a null `basisRef`. A row without a `basis` field
  (structural, declaration and call evidence) has no `basisRef`. The table is per
  page and lists only the bases that page's rows use. Re-inlining it gives back the
  rules.md `Evidence` row.
- **Status filter.** `trellis_rules_check` accepts
  `status?:("conforms"|"deviates"|"unknown")[]`, with 1–3 distinct values. An empty
  array, a repeated value or any other value is `invalid_request`. The filter keeps
  the `instances` and `testOnly` rows whose `status` it lists, excepted rows included
  (they keep their status), before the paged sequence is formed. `unassigned` and
  `externalPackages` are not filtered and still follow the instances in the sequence.
  - `counts`, `testCounts` and the other whole-result fields stay those of the whole
    evaluation.
  - `page` adds the required `status:("conforms"|"deviates"|"unknown")[]|null`, the
    filter as given, or null without one. `page.total` counts the filtered sequence.
  - The order of the kept items is unchanged. Offsets index the filtered sequence, so
    the same arguments at the same pin page stably. A different `status` is a
    different sequence.
  - The filter is MCP projection only. It never reaches evaluation, the CLI or the
    browser.
- **Instance limit.** For `trellis_rules_check`, `limit` (1–50, default **20**, the
  shared `defaultLimit`) is the most rule instances on one page, `instances` and
  `testOnly` together. The 24,576-byte page target still applies, so a page can hold
  fewer. `unassigned` and `externalPackages` items do not count toward it. Each check
  instance keeps **one** evidence entry, and `omittedEvidence` is the exact
  remainder; `trellis_rules_explain` shows more of one instance. Explain is otherwise
  unchanged: its `limit` (default 5) is still the evidence entries kept per instance.
  References are unchanged.
- **Measured.** This repository at `18813e60` has 657 files with a rust-analyzer
  import, and 3,530 rule instances, including 105 production and 108 test-only
  deviations. Before, finding every deviation meant paging the whole result: 697
  pages and 14.9 MB at `limit:5`, with 4,751 inline bases. With
  `status:["deviates"],limit:50` it takes 18 pages and 412 KB. The production
  deviations fill the first 8 pages (189 KB), each with one basis entry and none
  inline.

## Amendment (#715): compact find and search, result-language scope rows

From the 2026-11-05 dogfood: `trellis_search_symbols` returned about 7 KB for six hits,
most of it context repeated on every call: four `scopeCoverage` rows whatever the result
languages, the same Rust macro-gap diagnostic on every coverage row, and proof detail an
agent rarely reads. `toolVersion` is **11** (previously 10). This section wins over the W2
`scopeCoverage`, `coverage` and data text, the #211 search data text, and the #528
language-status sentence, for find and search only. Matching, ranking, limits, the
response-byte fit rule, completeness and warnings are unchanged. Describe `languages`,
incoming `scopeCoverage` and the lookup alternatives other tools return keep their shape.

- **Result-language scope rows.** In full and compact find (query and lookup key) and
  search, `scopeCoverage` holds the `LanguageStatus` rows of the emitted items' languages
  only, in #22 order. With no emitted item it holds the searched languages with selected
  native tuples (the #528 `syntaxOnly` rule), and with neither it is empty. Row contents are
  unchanged. `partial` and the aggregate `coverageIncomplete` warning still cover every
  searched language, so an answer can be partial because of a language it does not list.
  Because the empty prefix lists more rows than a hit prefix may, the W2 byte fit no longer
  treats it as a lower bound: the longest fitting hit prefix is returned even when the
  empty prefix alone would not fit, and `range_too_large` means neither the empty prefix
  nor the first hit fits.
- **Coverage diagnostic table.** Full find and search data add the required
  `coverageDiagnostics:Text[]`: each distinct coverage `diagnostic` of the page's rows once,
  in first-row order. Each `coverage` row carries `diagnosticRef:UInt|null` instead of
  `diagnostic`, null for a row without one. Re-inlining the table gives back the native
  `Coverage` row. The table is per page and empty when no row has a diagnostic.
- **Compact find and search.** Both accept `compact?:boolean` (default false; a
  non-boolean is `invalid_request`), find with either `query` or `lookupKey`. When true, data
  omits `documents`, `coverage`, `coverageDiagnostics` and `provenance`. Each item keeps only
  `symbolId`, `name`, `stableLookupKey`, `kind`, `role`, `path` (the declaration's document
  path, in place of `doc`), `lines` and `evidenceTier`, and search hits also keep `rank` and
  `matchedBy`. For the same emitted prefix, the items, `scopeCoverage`, search's
  `searchIndex` and `role`, warnings, `partial` and `truncated` are those of the full answer.
  The byte fit uses the smaller compact frame, so more items may fit, and `truncationReason`
  can be `limit` where the full answer is `response_bytes`. Byte ranges, display keys,
  revisions and proofs need the full answer.
- **Compact graph node keys.** Each compact `trellis_call_graph` node declaration adds the
  required `stableLookupKey:Text|null`, the node's #593 stable lookup key, or null when it
  has none (a partial Rust impl parse). This is the cheap, durable handle for follow-up
  calls; the short `lookupKey` stays. Full graph declarations are the contract records and
  do not carry it. Default limits are unchanged. On the #665 fixture shape the keys add
  about 3 KB to a fifty-node compact answer, which still fits by `limit`.
- **Catalog.** Search's output schema drops its unreferenced copy of `Symbol` (`SearchHit`
  already repeats its fields), which keeps `tools/list` inside its named headroom. No
  response shape depends on it.
- **Measured.** A private clone of this repository at `cd2f0a9a` with a rust-analyzer
  import, as complete JSON-RPC lines (bytes, items):

  | Call | Before | After, full | After, compact |
  |---|---|---|---|
  | search, production role, "decide whether a checkout is still catching up after an edit" | 15,785 (10) | 14,675 (10) | 4,813 (10) |
  | search "stable lookup key" | 16,882 (10) | 15,191 (10) | 4,155 (10) |
  | search "frame bytes response cap", limit 20 | 21,824 (20) | 21,103 (20) | 8,150 (20) |
  | find "navigation_at" | 6,490 (3) | 5,360 (3) | 1,896 (3) |
  | find "catching_up" | 23,074 (20) | 21,292 (20) | 6,821 (20) |
  | find "symbols", limit 50 | 64,481 (46, `response_bytes`) | 64,731 (49, `response_bytes`) | 15,965 (50, `limit`) |
  | find by lookup key `src/mcp/projection.rs::compact_graph` | 4,080 (1) | 2,949 (1) | 1,298 (1) |
  | compact call graph `src/http/worker.rs::DaemonState::enqueue_index` | 6,924 (5 nodes) | n/a | 7,260 (5 nodes, all keyed) |

## Amendment (#770): a bounded prompt reference view

**Status: owner-ratified 2026-11-07 (#770).** `trellis_propose_rules`
inlined the whole `references:v2` result, so on any non-trivial checkout the prompt
exceeded the 65,536-byte frame and failed with `response_too_large`. This section wins
over the [proposal prompt](#one-no-argument-proposal-prompt) and
[rules proposal workflow](rules.md#proposal-workflow) text only for what "structural
references" means inside the prompt. The tools, their pages, the pin, the rules bytes and
descriptor, the atomic failures and the frame are unchanged.

The prompt is a hybrid: a small fixed context plus an explicit workflow that pages the
existing tools for detail.

- **Unchanged:** the server instructions and the full rules grammar (skeleton, external
  policy, exceptions); the current exact `rules.toml` bytes and descriptor, or explicit
  absence; the pin (`evidenceBasis`), `workspace` and `catchingUp`.
- **`payload.references` is a summary** of the pinned references result, not the result.
  - It keeps `referencesVersion`, `evidenceTier`, `semanticComplete`,
    `semanticProducers`, `partial`, `counts` (scanned documents, mapped, unresolved and
    semantic sites) and `limitations` as they are.
  - It adds
    `totals:{productionEdges,productionSites,testEdges,testSites,externalPackages,partialFiles}`.
- **One overview table,** `overview:{rows,total,omitted}`.
  - Each row is `{source,target,sites}`: production native edges summed by source and
    target directory. A file's directory is its path up to the last slash, or `.` at the
    root. Package destinations have no directory and are only counted.
  - Rows are ordered by `sites` descending, then by source and target.
  - The overview keeps the longest prefix whose rows fit a fixed budget of 12,288 bytes
    (`PROMPT_OVERVIEW_BYTES`). The rows' bytes are their JSON array as escaped in the
    prompt text, with separators and without brackets. If large rules bytes leave less
    room, it keeps the longest prefix whose complete response line still fits the frame.
    Both limits are measured on the serialized bytes. `rows.length + omitted = total`,
    and the result is deterministic for one pin and rules text.
  - Why 12 KiB: a directory row costs about 45 to 70 bytes, so the budget holds roughly
    200 rows. That covers every production directory pair of mimir (74) and Trellis (67).
    It also keeps the prompt near the #528 24 KiB agent page target.
- **Not listed:** test, package and file-module edges, site samples, module and package
  descriptors, unresolved sites and partial files. They are counted, not listed.
- **Workflow.** The instructions tell the agent to:
  1. start from the overview;
  2. page `trellis_references` (`expectedBasis`, `paths`, `offset`) to refine particular
     areas;
  3. draft `rules.toml` and test it with `trellis_rules_check` /
     `trellis_rules_explain`;
  4. pull exact file-level edges from `trellis_references` only when writing baseline
     exceptions.

  A future `groupBy` aggregation
  ([#775](https://github.com/SquashMerge/trellis/issues/775)) will make step 2 cheaper.
- **Atomic failure stays.** Rules bytes are never clipped. If the fixed part
  (instructions, rules bytes, pin and envelope) does not fit, the prompt still fails
  atomically with `response_too_large`.

## Amendment (#769): rules tools point to the next call

**Status: owner-ratified 2026-11-07.** From the 2026-11-07 setup of a new repository and a
Claude Desktop session. With no rules file, nothing named the prompt that drafts one.
A check instance's single kept evidence row gave no route to the rest, so the agent
read source files instead. Desktop also could not call `trellis_rules_explain` at all,
because it sent a `{language,path}` module pair as `target` and got only `Invalid tool
arguments`. `toolVersion` is **13** (previously 12). This section wins over the #129
failure text, the #725 instance text and the failure table below. Evaluation,
verdicts, counts, `omittedEvidence`, instance order, paging rules and bounds are
unchanged, as are the CLI and the browser.

- **Rules file hint.** The `rules_not_found` message from check and explain is the
  static text ``No .trellis/rules.toml. Ask the user to run the
  `trellis_propose_rules` MCP prompt (in Claude Code:
  /mcp__trellis__trellis_propose_rules) to draft one for review.`` Its code and
  retryability are unchanged.
- **Ready-made explain call.** A `trellis_rules_check` instance (in `instances` or
  `testOnly`) with `omittedEvidence > 0` adds the optional
  `nextCall:{tool:"trellis_rules_explain",arguments}`. `arguments` is
  `{schemaVersion:1,rule,target,destination?,limit}`. `rule` and `target` are copied
  verbatim from the instance, and so is `destination` when the instance's rule is
  `$layers` or `$external` and its destination is non-null. `limit` is the
  instance's evidence count, `1 + omittedEvidence`, at most 50.
  - Sent back as given, these arguments validate. The explain answer lists that
    instance with up to `limit` evidence entries, all of them when the count is 50
    or less. A caller that chose a `workspace` for check adds the same one. It may
    also add the check's `evidenceBasis` as `expectedBasis`, to read the same pin.
  - An instance whose arguments would not validate as an explain request carries no
    `nextCall`. Neither does an instance with `omittedEvidence:0`. Explain instances
    never carry one.
  - The hint is server-authored from the instance's own identities and adds no
    repository text. It is part of the instance it rides on, so the page fit and the
    instance `limit` measure each instance with its hint. A page can therefore hold
    fewer instances than before, but no instance is split, dropped or clipped.
- **Field-naming argument errors.** An `invalid_request` from application argument
  validation now says which field failed and what it expects, as static server text.
  It never echoes the caller's arguments or repository content. Validation order,
  codes and retryability are unchanged, and so is every other code, including
  `range_too_large`, which keeps its generic message.
  - The message names the first failing check: first an argument the tool does not
    accept (`Invalid tool arguments: a field is not in this tool's inputSchema`),
    then a missing required field, then each field in the existing validation order.
    Non-object arguments say `Tool arguments must be a JSON object`.
  - For example, an explain `target` that is not a check identity gives `` `target`
    must be the identity object from a trellis_rules_check instance: {sourceSetId,
    language, path, sid} ``.
  - An object-valued field (`target`, `destination`, `expectedBasis`) that arrives as
    a JSON string, as a client that stringifies object arguments sends it, says so:
    `` `target` must be a JSON object (got a string): … ``. The server still rejects
    it; it never parses a string as an object.
  - The exact text of each message is server-authored, not part of this contract.
    Each one names its field in backticks and fits the 512-byte summary.
- **Object input properties.** In every tool's `inputSchema`, a property that is a
  bare `$ref` to an object-valued definition also carries `"type":"object"` (allowed
  beside `$ref` in JSON Schema 2020-12). This covers `expectedBasis` on the ten tools
  that accept it, and explain's `target` and `destination`, because some clients
  choose how to serialize an argument from its `type`. The same holds at any depth of
  an input schema: explain's `Destination` union and each of its `oneOf` branches say
  `"type":"object"` too. Accepted values are unchanged.
- **Catalog.** Check's output `RuleInstance` adds the optional, closed `nextCall`.
  `trellis_read_source`'s output schema drops its unreferenced `SemanticProjection`,
  `DocumentSemanticStatus` and `Language` definitions, which no response shape uses.
  The complete catalog is 31 bytes smaller than at `toolVersion` 12, so it keeps its
  named headroom.

## Amendment (#783): per-app layers in the proposal prompt

**Status: owner-ratified 2026-11-07 (#783).** Following the
[owner's 2026-11-07 ruling](https://github.com/SquashMerge/trellis/issues/783),
`trellis_propose_rules` gives per-app context and asks for per-app layers, under
[rules.md Amendment (#783)](rules.md#amendment-783-apps-and-per-app-layers). Everything in
[Amendment (#770)](#amendment-770-a-bounded-prompt-reference-view) is unchanged except
what this section adds.

- **`payload.apps`** lists the detected apps: `{name, kind:"cargo"|"npm"|"python" (python added by owner ruling 2026-11-08), manifest, roots,
  entries}`, ordered by manifest path. It is untrusted repository data, like the rest of
  the payload.
  - The prompt detects apps against the current rules bytes, so a manifest that a hidden
    layer selects is left out.
  - Without a rules file, every manifest is listed. Placing tooling and fixture manifests
    in hidden layers is part of the agent's proposal.
  - `apps` belongs to the fixed part of the prompt. Like the rules bytes, it is never
    clipped, and if the fixed part does not fit the prompt fails atomically with
    `response_too_large`.
- **Instructions.** The instructions ask the agent to:
  - propose layers per app. When two or more apps are detected, every ordered layer names
    its `app`, and a shared layer lists the apps it serves in `apps` (or omits it to serve
    every app);
  - number each app's `order` on its own;
  - put tooling, test harnesses and fixtures in `display = "hidden"` layers of the app they
    belong to, with fixtures in the app whose tests consume them;
  - leave references between apps alone. They report `unknown`/`cross_app`, so they are
    not deviations and need no exception.

  With zero or one app, the instructions are as before.
- **Unchanged:** the overview table, its budget and order, the tools, the pin and the
  descriptor. Overview rows stay directory pairs and are not split by app.

## Amendment (#683): TypeScript language rows

**Status: owner-ratified 2026-11-07 ([Decision 0020](semantic-evidence/decisions/0020-typescript-language.md) item 7).** `Language` gains `typescript` after `javascript`; #22 order is java, rust, python, javascript, typescript. Fixed `LanguageStatus` rows (describe `languages`, incoming `scopeCoverage` and the lookup alternatives other tools return) are five. Find and search keep the #715 result-language rows, so a `typescript` row appears exactly when TypeScript items are emitted, or, with none emitted, when TypeScript has selected native tuples in the searched scope. `Destination.language` accepts `typescript`. npm and Node descriptors serve TypeScript importers, and their identity language is the importer's. `toolVersion` is unchanged.

The added schema bytes are paid for without changing what any schema accepts: each tool's output `$defs` drops the `FailureCode` definition, which no response shape references (`ResolvedFailureCode` is the one `Failure.error.code` names, and still omits `workspace_selection_failed`). The catalog headroom of Decision 0011 item 6 is unchanged.

## Amendment (#785): classified rule evidence

**Status: proposed (owner-delegated, run 802).** `toolVersion` is **15** (previously 14, #777). The change is additive on the wire apart from the two documented behaviours below; paging, `omittedEvidence`, `nextCall` and every bound are unchanged.

- **`Site`** gains the required `owner`, `target` and `targetReason`.
  - `owner` is `{syntaxId,name,stableLookupKey}|null`: the declaration that encloses the site. It is null only when `ownerSyntaxId` is null. `name` is null for an anonymous declaration or a module, and `stableLookupKey` is null when no key is recoverable and always for a file module.
  - `target` is `{syntaxId,displayKey,document,lines}|null`: the indexed declaration a fresh semantic binding resolves the site to. `targetReason` says why it is null: `structural_only` (no selected semantic evidence for the document), `external`, `ambiguous`, or `unresolved` (anything else, including a target outside this snapshot). `targetReason` is null if and only if `target` is non-null.
- **`relationKind`** is `call` for a structural callee path: a site whose range is the invocation or callee spelling of a measured call, or that a semantic call binding maps. An `import` is never promoted, so a JS or TS `require(...)` or dynamic `import(...)` stays `import` and may carry a `caller` naming the function that runs it. A structural path that a type-role binding maps is `type`. Without semantic evidence a Rust or Java type path stays `reference`. A `caller` is present only on `call` and `import` sites.
- **`Edge`** gains the required `kindCounts` `{call,type,import,reference}`, exact for every counted site, so the four add up to `count`. The sample takes an edge's `call` sites first, then the others, each group in its existing order, up to `limit`; `omittedSites` is still `count` minus the sites returned.
- **`RuleEvidence`** gains the optional `lines`, `relationKind`, `spelling`, `owner`, `target` and `targetReason`, and **`RuleInstance`** gains the optional `kindCounts`.
- **Catalog.** Each architecture tool's output `$defs` drops every definition its output root does not reach. Tool 8 drops `ConfigDescriptor`, `ConstraintResult`, `RuleCounts`, `RuleDiagnostic`, `RuleEvidence` and `RuleInstance`; tools 9 and 10 drop `Edge`, `Module`, `SemanticBasis`, `Site` and `UnresolvedReference`. No response shape used them, so what any schema accepts is unchanged. The complete catalog is 241,831 compact bytes before and 230,217 after; the `tools/list` frame is 230,574 bytes against the 245,760-byte named headroom limit. A test now checks that every output definition is reachable and every `$ref` resolves.
- **Frame.** An answer that does not fit one frame fails with `response_too_large`; ask for a smaller `limit`.

## Amendment (#714): rules check pages deviations first

**Status: proposed, awaiting owner ratification.** From the 2026-11-05 dogfood: with
76 production deviations among 615 unknowns, page 1 of `trellis_rules_check` held only
unknown `$layers` instances, because instances page in evaluation order (rule, target,
destination). `toolVersion` is unchanged at **15**: no field, type, bound or count
changes, only the order of rows within two arrays. This section wins over the #725
"order of the kept items is unchanged" sentence, for check only.

- **Status order.** Over MCP, `trellis_rules_check` orders `instances` by status,
  `deviates`, then `unknown`, then `conforms`, and orders `testOnly` the same way.
  Within one status, rows keep evaluation order. An excepted instance keeps its status
  and therefore its place. The arrays still form the #448 sequence `instances`,
  `testOnly`, `unassigned`, `externalPackages`.
- **Stable paging.** The order is applied before the `status` filter and before
  offsets are counted, so it is the same on every page: the pages concatenate to each
  row exactly once, `page.total` and the whole-result `counts` and `testCounts` are
  identical on every page, and the same arguments at the same pin give the same page.
- **`limit`** is unchanged from #725: the most rule instances on one page, `instances`
  and `testOnly` together.
- **Unchanged:** evaluation, verdicts, counts, `omittedEvidence`, `nextCall`, bounds,
  `trellis_rules_explain` (which pages its instances in evaluation order), the CLI and
  the browser.
- **Not in this amendment:** grouping or skipping `layer_unassigned` unknowns.

## Amendment (#798): a typed `building` answer from the architecture tools

**Status: proposed (owner ruling 2026-11-08 on #798, Options 1 and 2a; needs ratification of this text).** `toolVersion` is **16** (previously 15, #785). This section wins over the #129 text and the [One computation per revision](architecture-view.md#one-computation-per-revision-556) table where they differ.

- **The answer.** While the checkout's Architecture owner is still computing the read's revision, `trellis_references`, `trellis_rules_check` and `trellis_rules_explain` fail with the new code `building` instead of `deadline_exceeded`. Its resolved failure has the ordinary fields, with `retryable:true` always, plus the required `progress` and `retryAfterMs`:

  `{schemaVersion:1,requestId,workspace,catchingUp,error:{code:"building",message,retryable:true,currentBasis:Basis,currentContentHash:null,progress:{phase:"reading"|"resolving"|"evaluating"|"measuring",completed:UInt,total:UInt}|null,retryAfterMs:UInt}}`

  `progress` is null when the build has not reported yet. The message names the phase and count, for example `Architecture is being computed for this revision (measuring 120 of 670); retry shortly`. `retryAfterMs` is 250, the browser's `202 building` value. It is never a partial answer. The `trellis_propose_rules` prompt returns the same error object as its JSON-RPC `-32603` error `data`.
- **When.** A read waits on the owner's signal until one second before its 5 s deadline, then answers `building` if the core is still not published. A read whose revision the owner has passed (the head moved) waits again for the new head while time remains.
- **No duplicate compute.** A read with `paths` waits like one without, and answers `building` while the owner builds its revision. It no longer starts its own whole-revision read beside the build. It still computes its own scope when the revision fails `bounded_work`.
- **Progress.** `measuring` advances per measured document, as `reading` already did, so a retried read sees the count move.
- **Eager build.** The owner builds every active runtime's new head after each publication and at activation, with or without `.trellis/rules.toml`, so most first calls after a restart are hits. A worker step hints the owner only when the head or the rules file differs from the last step's, so an unchanged step takes no extra read lease.
- **Catalog.** Each of the three tools' output schemas gains a `BuildingFailure` `$defs` entry and a `oneOf` branch for it. `building` is not in `ResolvedFailureCode`, so the plain `Failure` branch never matches it. The compact catalog grows from 230,217 to 233,139 bytes, under the 245,760-byte named headroom limit.
- **Clients.** `tools/mcp-smoke` reads a `building` tool call or prompt again after `retryAfterMs`, for at most 60 s per call, and reports the count as `buildingRetries`.

## Amendment (#818): `trellis_show` takes the user's viewers to a target

**Status: proposed, for ratification under the 2026-11-08 owner delegation on #818.** `toolVersion` is **17** (previously 16, #798). The 2026-11-08 owner rulings on #818 set the scope: views `sequence`, `class` and `source`; every open viewer of the checkout follows; following is on by default and a pin opts out; validation as the #787 deep links validate; typed answers, never partial.

- **One new tool.** `trellis_show` is entry 12 of the catalog, after `trellis_rules_explain` ([catalog entry](#trellis_show-818)). The eleven earlier tools, their schemas and their answers are unchanged. With #798's catalog it is 233,137 compact bytes before and 241,268 after; the modern `tools/list` frame is 241,625 bytes against the 245,760-byte named headroom limit.
- **What it changes.** Only what open viewers of the checkout display. It never writes the index, the rules, `requests.db` or any saved state, and it never moves terminal or window focus. Its `readOnlyHint` is true; `idempotentHint` is false, because each call is a new request that an open viewer follows again.
- **One latest request per checkout.** A shown target is recorded in the daemon's memory for its checkout, under a `generation` that is strictly increasing across the daemon process and starts at the process's start time in milliseconds, so a restarted daemon's requests are newer than an earlier one's. A newer request replaces the older one; there is no queue. A checkout runtime that is released forgets its request.
- **Viewers.** A viewer is a browser tab or Herdr pane that read the checkout's `status` or `status/head` in the last 90 seconds; each such read also marks the viewer as open. Both routes gain `viewRequest`: `null`, or `{generation,view,symbolId,revision}` for `sequence` and `class`, or `{generation,view:"source",path,line,revision}`. `revision` is the snapshot the target was checked at. The `status/head` digest, and so its `ETag`, includes it. These are browser routes ([daemon-v1](daemon-v1.md)), not MCP output.
- **Following.** A viewer treats the first `viewRequest` it sees for a checkout as a baseline and follows each newer generation unless it is pinned. A pinned viewer counts the request as seen, so unpinning does not replay it. A viewer at another snapshot than `revision` does not navigate; it says the agent showed a target at another snapshot (the browser offers Reload and show). A viewer that follows shows a "Shown by agent" note with Back, which returns to the view that was visible when the show started. A show owns the viewer only until a newer show, a user navigation or a pin; one that loses ownership while it reads changes nothing. A shown Sequence is bound to its revision: a new revision reads it again, and Back never shows an older revision's payload.
- **Not in this amendment:** architecture focus, a viewer count, per-viewer targeting, Classes or source in the Herdr pane (it shows a note for those two views).

## Amendment (#780): describe pages the failed and partial semantic documents

**Status: proposed, for ratification of the approved #780 scope.** `toolVersion` is **18** (previously 17, #818), shared with [Amendment (#775)](#amendment-775-references-group-by-directory-or-package). From the 2026-11-07 dogfood: describe reported 33 Rust documents as `failed: 33 · not in producer output` and gave no way to learn which. The `semantic.documents` array is the #620 per-document projection status, not this list. This section wins over the W1 input and data text.

- **Input.** Describe accepts the optional `coverageDocuments:{language?:Language,limit?:UInt,offset?:UInt}`. An empty object asks for every language. `limit` is 1–50 and defaults to 20; `offset` is any safe UInt. An unknown field, a wrong type or an unknown language is `invalid_request`; a `limit` out of range is `range_too_large`.
- **Output.** When the call asks and the head is ready, data adds `coverageDocuments:{items:CoverageDocument[],page:{offset,nextOffset,total}}`, with `CoverageDocument={language,path,state:"failed"|"partial",reasons:Reason[]}`. Without the request, or with no ready head (`languages:[]`), the field is absent, so an ordinary describe answer is unchanged apart from `toolVersion`.
- **Which documents.** Exactly the tuples behind each language's `semantic.coverage.failed` and `partial` counts in the same answer: the selected failed or partial tuples of the language's default producer (#483) at the admitted head, plus #620 older-basis documents whose basis tuple is failed or partial. With `language`, `total` equals that language's `failed + partial`; without it, the sum over the five languages.
- **Reasons.** A failed document has one reason, from its stored diagnostic, in the buckets of the #520 summary: `notInProducerOutput` (`excluded_failed_by_producer`), `unmeasurableSourceAnchor`, `rawCaptureUnavailable`, else `other`. A partial document has each recorded #520 reason, named as the `partialReasons` counts: `nativeGaps`, `capped`, `omitted`, `capturedDiagnostics`, or `unrecorded` alone when the import recorded none. Reasons are import metadata, not evidence; the free-text diagnostic is not returned.
- **Order and paging.** Documents sort by #22 language order, failed before partial, then path bytes. A page holds the documents from `offset`, at most `limit`, while the complete JSON-RPC response line (every field of the final answer, including `searchIndex`, `workspace` and the summary, plus LF) stays inside the 24,576-byte page target (at least one document). `nextOffset` continues it and is null on the last page; an `offset` past `total` fails `range_too_large`. Describe takes no `expectedBasis`: each page reports its `evidenceBasis`, and a caller that sees it change starts again from 0.
- **Unchanged:** the five `LanguageStatus` rows, `partialReasons`, `partial`, warnings, and the CLI `trellis status`.
- **Choices beyond the issue text.** The camelCase reason codes (matching the `partialReasons` keys rather than the stored snake-case diagnostics), the opt-in input object, and paging without a basis pin.

## Amendment (#775): references group by directory or package

**Status: proposed, for ratification of the approved #775 scope.** `toolVersion` is **18**, shared with [Amendment (#780)](#amendment-780-describe-pages-the-failed-and-partial-semantic-documents). From the mimir setup (2026-11-07): to see a repository's shape an agent had to page every file-level edge. This section wins over the #129 references input and paging text where they differ; check, explain, the prompt, the CLI and the browser are unchanged.

- **Input.** `trellis_references` accepts the optional `groupBy:"file"|"directory"|"package"` (default `file`, today's answer) and `depth` (1–32), which only `directory` accepts. Another `groupBy` value, or `depth` without `directory`, is `invalid_request`; an integer `depth` out of range is `range_too_large`. `paths`, `expectedBasis` and `offset` keep their meaning: `paths` still selects sources before anything is measured.
- **Groups.** A source is always a native module. Its group is its directory (the path up to the last `/`, `.` at the checkout root), cut to its first `depth` segments when `depth` is given; with `package` it is its `sourceSetId`. A native destination is grouped the same way. An external destination is its own group: the `ExternalIdentity` object, so package rows match `externalPackages` exactly in both modes.
- **Rows.** Data adds the required `groupBy` (the mode used) and `groups:{source:Text,target:Text|ExternalIdentity|null,sites,production,test}[]`. A row sums the exact `count` of every file-level edge with that source and target group, split by the production and test arrays they came from, and `sites = production + test`. Unresolved sites are rows with `target:null`, one site each, split by the site's own role (`test`, else production), so aggregation never hides unknowns. A group's references to itself are a row like any other. Rows sort by `sites`, largest first, then by source bytes, then target (native group, then external identity, then null). In `file` mode `groups` is `[]`.
- **What stays.** With `directory` or `package`, `modules`, `production`, `tests` and `unresolved` are `[]` (no file-level item is emitted, so no module closure is needed); `externalPackages` keeps the descriptors of the emitted external destinations. `counts`, `partialFiles`, `limitations`, `semanticProducers`, `semanticDocuments`, `partial`, `semanticComplete:false`, warnings and the pin are those of the file answer. The sums equal the file totals: production rows with a non-null target add up to the production edges' counts, and null-target rows to `counts.unresolvedSites`.
- **Paging.** `groups` is the last array of the references page sequence (`modules`, `externalPackages`, `production`, `tests`, `unresolved`, `groups`), and pages as #448 describes. In a grouped answer `limit` is the most rows on one page (as check's #725 instance limit); without it a page fills to the page target. In `file` mode `limit` stays the sample sites per edge.
- **Catalog.** Describe's input gains `coverageDocuments` and its data the optional `coverageDocuments`; references' input gains `groupBy` and `depth`, and its data `groupBy` and `groups`. With #780 the compact catalog grows from 241,270 to 243,301 bytes, and the modern `tools/list` frame is 243,658 bytes against the 245,760-byte named headroom limit.
- **Choices beyond the issue text.** `package` groups native modules by source set (today one per checkout) because the references payload carries no build-manifest package for a native file; grouping by the #783 manifest apps is a possible follow-up. Rows also carry `production` and `test` beside `sites`, and `kindCounts` and semantic site counts are not summed.

## Amendment (#839): workspace selects any local Git checkout

**Status: owner-ratified 2026-11-09 ([#839](https://github.com/SquashMerge/trellis/issues/839): “Yes, we should relax W8”).** `toolVersion` is **19** (previously 18, #780/#775). This section wins over the selection text above, W8 and [Decision 0007 §2](semantic-evidence/decisions/0007-user-daemon-and-workspace-selection.md#2-per-call-workspace-selection-amends-24) where they differ. Agents (Claude subagents and feature-factory specialists) work in `git clone --local` sandboxes and their linked worktrees while sharing one `trellis mcp` launched in the owner's checkout; the same-repository rule kept their answers on the launch checkout and protected nothing under T00, since the same user can already index and query any checkout from the CLI.

- **Selection.** An explicit `workspace` is a verified absolute directory path that resolves to a Git checkout root, without silently following a symlink to a different selection. The selected checkout's Git common directory no longer has to be the launch checkout's.
- **Unchanged.** Root identity checks at admission and at the send decision (each root's captured Git common directory must still be the one it had at selection), one checkout per result, attachment for the session's lifetime, the `not_absolute`, `not_checkout`, `identity_changed` and `unavailable` reasons, and Decision 0009 checkout capacity (`checkout_capacity`).
- **Removed.** `different_repository` leaves the `workspace_selection_failed` reason enum of every tool's output schema. There is no compatibility shim.
- **Catalog.** Removing the enum value from the twelve output schemas shrinks the compact catalog by 276 bytes; the modern `tools/list` frame is 243,382 bytes (243,658 at `toolVersion` 18) against the 245,760-byte named headroom limit.

## Amendment (#784): per-app counts in rules check

**Status: proposed (owner-delegated, run 803).** `toolVersion` is **20** (previously 19, #839). Detection and the counting rule are in [rules.md implementation notes (#784)](rules.md#implementation-notes-784). This section wins over the #129 description of the check payload where they differ.

- **`trellis_rules_check`** data gains two required fields, whole on every page like `counts`:
  - `apps`: `[{name:Text,manifest:Path,counts:RuleCounts}]`, one entry per detected manifest in manifest-path order. Apps that share a name each keep their own entry. An entry carries no `kind`, roots or entries; those are in the proposal prompt's `payload.apps` and the browser summary. App kinds are `cargo`, `npm` and `python`; `python` apps are detected from `pyproject.toml` ([#807](https://github.com/SquashMerge/trellis/issues/807)).
  - `noApp`: `{counts:RuleCounts}`.
- **Counts** cover production instances, field by field, so `Σ apps[].counts + noApp.counts = counts`. A `cross_app` instance is `unknown` and counts under `noApp`. With no detected app, `apps` is `[]` and `noApp` holds every count.
- **`trellis_rules_explain`** does not carry `apps` or `noApp`; its schema and output are unchanged.
- **Single-app output.** With exactly one detected app, `apps[0].counts` equals `rules.counts` only when no row's own file is unassigned and outside the app's root. Such rows count under `noApp`. So a single-app checkout whose app root is a subdirectory, and which has unassigned files outside that root, reports part of its CLI and MCP per-app output under `noApp`.
- **Limitation.** When manifests are unread, `limitations` gains the one-line unread-manifest entry described in rules.md.
- **Unchanged:** paging, `omittedEvidence`, `nextCall`, every bound, the check input, and the `trellis_propose_rules` payload of [Amendment (#783)](#amendment-783-per-app-layers-in-the-proposal-prompt). `trellis rules check --format json` prints the same two fields and its exit rule is unchanged.

## Amendment (#836): catalog compaction

**Status: owner-ratified 2026-11-09 under the owner's delegation on [#836](https://github.com/SquashMerge/trellis/issues/836): “Let's compact the MCP tools list, you can amend the contract.”** `toolVersion` is **21** (previously 20, #784), because the catalog text changes. No tool accepts or emits anything different: every input and output schema accepts exactly the instances it accepted at `toolVersion` 20. Only the schema text changes. This section wins over the #798 catalog text (the `BuildingFailure` definition and branch) and over any definition name in earlier sections where they differ.

- **Why.** At `toolVersion` 20 the modern `tools/list` frame was 243,767 bytes (legacy 243,710) against the 245,760-byte named headroom limit (Decision 0011 item 6: the 262,144-byte catalog cap minus 16,384), leaving under 2 KB for the next change.
- **Self-contained schemas.** Each `inputSchema` and `outputSchema` stays one self-contained JSON Schema whose every `$ref` is local (`#/$defs/…`). The catalog has no shared, cross-tool or remote definitions: MCP clients compile each tool's schema on its own (the pinned TypeScript SDK passes each `outputSchema` to Ajv separately), and the 2026-07-28 basic specification forbids dereferencing a network `$ref` by default. Deduplication is therefore within one schema only.
- **Compaction rules.** Each preserves meaning on its own:
  1. **No `$schema`.** Schemas omit `$schema`. MCP fixes JSON Schema 2020-12 as the dialect of a schema without one, which is the dialect they declared.
  2. **One failure without a workspace.** The four failures that carry no `workspace` (workspace selection, invalid workspace, admission, arguments) share one closed `NoWorkspaceFailure` envelope `{schemaVersion,requestId,error}`. Its closed `error` lists the union of their fields and a `oneOf` of the four shapes:
     - `workspace_selection_failed` with `attemptedWorkspace` and `reason`, `retryable` true exactly for `unavailable`;
     - `invalid_request` with `attemptedWorkspace`, not retryable;
     - `too_many_requests`|`deadline_exceeded` with `attemptedWorkspace`, retryable;
     - `invalid_request`|`range_too_large` without `attemptedWorkspace`, not retryable.

     `SelectionFailure`, `InvalidWorkspaceFailure`, `AdmissionFailure` and `ArgumentFailure` leave `$defs`. Every tool's root `oneOf` names `NoWorkspaceFailure` once, last.
  3. **Nested resolved failures.** On the three architecture tools, the located `invalid_rules` and the #798 `building` errors share the resolved `Failure` envelope: `Failure.error` is a `oneOf` of the resolved error, `LocatedRuleError` and `BuildingError`. `LocatedRuleFailure` and `BuildingFailure` leave `$defs` and the root `oneOf`.
  4. **Shared fragments.** A subschema repeated within one schema moves to a `$defs` entry when that saves bytes: `TextOrNull`, `UIntOrNull` and `RangeOrNull` (a definition or null), `CoverageCounts` (the five `LanguageStatus` coverage counts), `Texts`, `Freshness` and `EvidenceKind`. An inline `{start,end}` object is a `$ref` to the identical `Range` (or `LineSpan` where there is no `Range`), and `LineSpan` is `{$ref:Range}` where both exist.
  5. **Null folded into the type.** `anyOf:[{enum:E},{type:"null"}]` becomes `{enum:E+[null]}`, `anyOf:[{const:c},{type:"null"}]` becomes `{enum:[c,null]}`, and a nullable string with only string keywords becomes `{type:["string","null"],…}`.
  6. **Shorter Basis pattern.** `indexGeneration` uses the equivalent `^(?!0{8}-0{4}-0{4}-0{4}-0{12}$)[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}(?![\s\S])`.

  Descriptions are unchanged.
- **Sizes.** The compact catalog is 243,410 bytes before and 220,646 after. The modern `tools/list` frame is 221,003 bytes (legacy 220,946), 22,764 bytes smaller. By rule: failures 15,867, `$schema` 1,368, fragments 1,760, `Range` 1,512, nullable definitions 846, null folding 767, pattern 644. A test keeps the frame at most 225,280 bytes: the named headroom plus 20,480 bytes of the recovered space. A change that needs more amends this budget. **Amended by owner ruling 2026-11-09 (#851):** the limit is now 235,520 bytes (230 KiB), the named headroom plus 10,240 bytes of the recovered space, so that `trellis_endpoints` fits ([Amendment (#851)](#amendment-851-apps-in-describe-an-app-filter-and-trellis_endpoints)). The 245,760-byte named headroom limit is unchanged.
- **Checks.** Tests check that every schema is self-contained and has no `$schema`. They also check that the compacted failure schemas of all twelve tools accept exactly the failures the earlier shapes accepted, using an independent oracle over a matrix of codes, reasons, flags and fields, through the SDK's own validator. As before, real responses validate.
- **Unchanged:** every tool, input, output, bound and description, every definition not named above, and the catalog's order.

## Amendment (#794): authored apps and the `trellis_propose_rules` prompt

**Status: owner-ratified 2026-11-08 ([#794](https://github.com/SquashMerge/trellis/issues/794)).** It records the owner's 2026-11-08 rulings on #794: the prompt rename and widening ([ruling](https://github.com/SquashMerge/trellis/issues/794#issuecomment-6730845360)) and D1–D6 and D8 accepted as recommended ([ruling](https://github.com/SquashMerge/trellis/issues/794#issuecomment-6730859881)). `toolVersion` is **22** (previously 21, #836), because the `trellis_rules_check` output schema changes. Authored apps are [rules.md Amendment (#794)](rules.md#amendment-794-authored-apps). This section wins over [Amendment (#783)](#amendment-783-per-app-layers-in-the-proposal-prompt), [Amendment (#784)](#amendment-784-per-app-counts-in-rules-check) and the [proposal prompt](#one-no-argument-proposal-prompt) text where they differ.

- **Rename, with no alias.** The prompt `trellis_propose_layers` is now **`trellis_propose_rules`**. `prompts/list` has one entry, `{name:"trellis_propose_rules",description:"Propose reviewed rules (apps and path-glob layers) from pinned structural evidence; never writes rules.",arguments:[]}`, and `prompts/get` accepts exactly `{name:"trellis_propose_rules",arguments?:{}}`. The old name is an unknown name (`-32602`), as Trellis is pre-release. The result's `description` is "Read-only architecture rules proposal for human review". The `rules_not_found` message of the rules tools names the new prompt. Admission, bounds, the fixed and clipped parts and the failure behaviour are unchanged.
- **`payload.apps`** lists the effective inventory: `{name, source:"manifest"|"override", kind:"cargo"|"npm"|"python"|null, manifest:Path|null, roots, entries}`. With a rules file, the authored apps come first by name, with `kind` and `manifest` null and `entries` empty, then the detected apps they do not shadow, by manifest path. Without a rules file every detected manifest is listed, as before. The prompt does not check authored roots against the snapshot (that is `trellis_rules_check`'s `reference` and `ambiguity` errors), so a prompt still helps fix such a file. A malformed file still fails the prompt atomically. `apps` stays in the fixed, never-clipped part.
- **Instructions.** An `APPS` section is always present, whatever the number of apps. It says what `payload.apps` lists and its two sources; when to propose authored apps (a backend and a front end in one `package.json`, a server and a CLI in one package, a front end with no manifest, two manifests at one root); the `[apps.<name>]` schema with an example (`roots` only, its normalisation, an indexed document under each root); shadowing, nesting and the most-specific root; that an authored name must not equal a kept detected name and that renaming is authoring at the detected root; to keep authored tables as written unless the evidence shows they are wrong; and that a hidden layer selecting a manifest still removes that detected app. The closed grammar line lists `apps` among the top-level keys. The `PER-APP LAYERS` section, for two or more apps, is unchanged apart from its first sentence, which moved to `APPS`. This supersedes D7: the prompt may now draft `[apps]` entries for review.
- **`trellis_rules_check`** `apps[]` entries are `{name:Text, source:"manifest"|"override", manifest:Path|null, counts:RuleCounts}`, one per inventory item in inventory order. `source` is required. `trellis_rules_explain` is unchanged.
- **Catalog.** Only the `trellis_rules_check` `apps` item schema and the `toolVersion` constant change. The prompt list is not part of the tool catalog.
- **Unchanged:** every other tool, field and bound, `noApp`, the counting rule, paging, `omittedEvidence`, `nextCall`, and the overview table.

## Amendment (#837): references group by app, and a directory prefix

**Status: owner-ratified 2026-11-09 under the owner's delegation for the #836/#837 MCP work.** The owner approved the scope on [#837](https://github.com/SquashMerge/trellis/issues/837) on 2026-11-08 (“Yes we can group by app. Can't we also group by path? Programs are usually stored in a hierarchical file structure no matter the language.”) and said to start it on 2026-11-09. `toolVersion` is **23** (previously 22, #794). This section wins over [Amendment (#775)](#amendment-775-references-group-by-directory-or-package) where they differ; check, explain, the prompt, the CLI and the browser are unchanged.

- **`package` is the app.** With `groupBy:"package"` a native module's group is its [#784](https://github.com/SquashMerge/trellis/issues/784) app, from the same effective inventory as `trellis_rules_check` `apps[]` ([#794](#amendment-794-authored-apps-and-the-trellis_propose_rules-prompt)). A detected app is named by its manifest path (for example `web/package.json`), because two apps can share a name; an authored app, which has no manifest, is named `[apps.<name>]` (for example `[apps.console]`). A module in no app is in the group `noApp`. A manifest path always ends in its manifest file name (`Cargo.toml`, `package.json` or, with [#807](https://github.com/SquashMerge/trellis/issues/807), `pyproject.toml`) and an authored group in `]`, so no two kinds of group collide. External destinations keep their `ExternalIdentity` group, and unresolved sites stay `target:null`. Rows, sums, order and paging are as #775 describes.
- **The same assignment as check.** A file's app is the one the rules check and the Architecture summary give it ([rules.md, #784](rules.md#implementation-notes-784)). Apps are detected against the checkout's `.trellis/rules.toml`, so a manifest that a hidden layer selects is no app, and its authored apps shadow the detected apps under their roots. A file in a layer that serves exactly one app (an ordered or hidden layer's `app`, the only app, or a shared layer's single `apps` entry) is in that app, even outside its root. Any other file (unassigned, or in a shared layer serving several apps) is in the app whose root most specifically contains it, or in `noApp`. Without a rules file every detected manifest is an app and every file goes by root. With no detected app every module is in `noApp`. Only `package` reads the rules file. It evaluates the rules over the whole checkout, as an unfiltered `trellis_rules_check` does, whatever `prefix` and `paths` select, and takes each file's layer from that evaluation. So `package` fails `invalid_rules` exactly when check rejects the rules, including for an ambiguity only in a file the read excludes; the failure has no `diagnostic` (the references failure schema has none; `trellis_rules_check` reports the location). `file` and `directory` are unaffected.
- **`prefix`.** `trellis_references` accepts the optional `prefix:string`, a checkout-relative directory such as `src/store` or `src/store/`: `/` separators, at most one trailing `/`, at most 4096 bytes, and no empty, `.` or `..` segment, no leading `/`, backslash, `:` or NUL. Anything else (including a non-string) is `invalid_request`, whose message names `prefix`. It is a directory, not a glob: it selects the sources whose path lies under it (`src/store/a.rs`, not `src/storefront.rs` or `src/store` itself). Like `paths`, it selects sources before anything is measured, so `counts` and every array describe only those sources; with both, a source must match a `paths` glob and lie under `prefix`. Unlike `paths`, which keeps the checkout-wide list, it also scopes `partialFiles` and their limitation to files under it, so a prefix with no file under it reports none. `semanticDocuments`, `partial` and `semanticProducers` stay checkout-wide. Destinations are never filtered. A prefix with no source under it answers an empty result, not an error.
- **`prefix` with each `groupBy`.** It applies with `file`, `directory` and `package`. With `directory` and `depth`, `depth` counts segments below the prefix: a native group is its directory cut to the prefix's segment count plus `depth` (`prefix:"src/store", depth:1` gives `src/store/x`), and a native destination outside the prefix is cut at the same length. Without `depth` a directory group is the whole directory, with or without `prefix`.
- **Output.** No output field is added or changed: `groups` rows keep `{source:Text,target:Text|ExternalIdentity|null,…}`, and `groupBy` still names the mode.
- **Catalog.** References' input gains `prefix:{type:"string",minLength:1,maxLength:4096}` and its description names app groups, `noApp`, `prefix` and the relative `depth`. The schemas stay self-contained, with local `$ref`s only and no `$schema` (#836). The modern `tools/list` frame is 221,270 bytes (legacy 221,213), 188 bytes more than at `toolVersion` 22 (#794), against the 225,280-byte #836 compacted-frame limit and the 245,760-byte named headroom limit.
- **Choices beyond the issue text.** The manifest path or `[apps.<name>]` as the app group's text (names can repeat) and the literal `noApp` group; `prefix` with every `groupBy` rather than only `directory`; `prefix` filtering sources only, as `paths` does; `depth` relative to the prefix; and the diagnostic-free `invalid_rules`.

## Amendment (#851): apps in describe, an `app` filter, and `trellis_endpoints`

**Status: owner-ratified 2026-11-09 under the owner's MCP delegation.** [#851](https://github.com/SquashMerge/trellis/issues/851) is the MCP part of [#685](https://github.com/SquashMerge/trellis/issues/685) (apps) and [#686](https://github.com/SquashMerge/trellis/issues/686) (interfaces); the owner approved that split on 2026-11-09. The owner ruling of 2026-11-09 (#851) raised the #836 compacted-frame limit to 235,520 bytes so that `trellis_endpoints` is its own tool. `toolVersion` is **24** (previously 23, #837). This section wins over the W1 data text, [Amendment (#775)](#amendment-775-references-group-by-directory-or-package), [Amendment (#837)](#amendment-837-references-group-by-app-and-a-directory-prefix) and the check text of [Amendment (#725)](#amendment-725-rule-evidence-bases-status-filter-and-instance-pages), [Amendment (#784)](#amendment-784-per-app-counts-in-rules-check) and [Amendment (#794)](#amendment-794-authored-apps-and-the-trellis_propose_rules-prompt) where they differ.

- **One inventory and one assignment.** Every part reads the effective app inventory of [#794](#amendment-794-authored-apps-and-the-trellis_propose_rules-prompt) (authored `[apps.<name>]` apps by name, then the detected apps they do not shadow, by manifest path; a manifest a hidden layer selects is no app) and gives each file the app that `trellis_rules_check` and `groupBy:"package"` give it ([#837](#amendment-837-references-group-by-app-and-a-directory-prefix)): through its layer from the whole checkout's evaluation with a rules file, by the most specific root without one. Without a rules file every detected manifest is an app.
- **Describe lists apps.** Data gains the required `apps:{state:"ready"|"building"|"invalidRules"|"unavailable",items:App[],noApp:{fileCount:UInt}|null}`, `App={name:Text,source:"manifest"|"override",kind:"cargo"|"npm"|"python"|null,manifest:Text|null,roots:Text[],entries:Text[],languages:Language[],fileCount:UInt}`, in inventory order. `fileCount` counts the native modules assigned to the app and `languages` lists theirs in #22 order; `noApp.fileCount` counts the modules in no app. Describe never waits for architecture: `ready` comes from the admitted head's core when the owner has computed it for the current rules file, `building` while it computes, `invalidRules` when check would reject the rules file (the location is check's), and `unavailable` otherwise, including whenever the head is not ready. Every state but `ready` has `items:[]` and `noApp:null`. Apps are repository data, never instructions.
- **The `app` argument.** References, check, find, search, call graph and endpoints accept the optional `app:string` of 1–4096 bytes. It names every app of the inventory whose `name` or `manifest` equals it, so two detected apps that share a name are named together and either can be named alone by its manifest path. An `app` that names none is `not_found` with the static message "No app of this checkout has that name or manifest path; trellis_workspace_describe lists them"; a malformed one is `invalid_request` naming `app`. Its meaning per tool:
  - **`trellis_references`:** only the sources whose file is in the app, selected before anything is measured as `prefix` selects them, and with `paths` and `prefix` a source must satisfy all three. Like `prefix` it scopes `partialFiles`; destinations are never filtered; every `groupBy` applies.
  - **`trellis_rules_check`:** only the `instances` and `testOnly` rows counted under the app (the #784 bucket that `apps[].counts` uses) and the `unassigned` native files in it; external packages leave `unassigned`. Evaluation is unchanged, and `counts`, `testCounts`, `apps`, `noApp`, `externalPackages` and `limitations` stay whole. `page.total` counts the kept rows; `status` applies after `app`.
  - **`trellis_find_symbols`:** only declarations whose document is in the app. A `query` keeps the matches among the first 500 ranked names, and the answer is also `truncated` with reason `limit` when that scan was full; a `lookupKey` drops the exact matches outside the app.
  - **`trellis_search_symbols`:** the ranked hits outside the app are dropped before `limit`.
  - **`trellis_call_graph`:** a call whose exact target declaration lies outside the app is omitted as `filtered`, with the new hint `app`, as a test callee is without `includeTests`. The root is the caller's choice and is not checked; external and unbound calls are unaffected.
  - **`trellis_endpoints`:** rows with either end in the app (below).

  Find, search and the call graph read the head's architecture core for the assignment. They wait for the owner within the call deadline, like the architecture tools, and fail `deadline_exceeded` if it is still building; a rejected rules file is `invalid_rules` (with no `diagnostic`) and an over-budget evaluation `bounded_work`. Their `ResolvedFailureCode` gains these two codes. Without `app` nothing changes.
- **`trellis_endpoints`.** A new architecture tool, entry 13 of the catalog, after `trellis_show` ([catalog entry](#trellis_endpoints-851)).
- **Extraction without a rules file.** The core now extracts interfaces whenever detected plus authored apps number two or more, with or without a rules file, so `trellis_endpoints` answers rule-less checkouts. The browser summary, which needs rules, is unchanged. A read that assigns apps or serves endpoints takes only a core built under exactly the checkout's current rules state: the current file's, or no file once it is removed (and the owner builds the right one while the read waits). A core built under a removed or other file never answers it, so its extraction never reaches an answer whose `extracted` is decided by the current file; the endpoint stage refuses such a core (`store_unavailable`) rather than projecting it.
- **Catalog.** One change: describe's `apps` and description, the five `app` inputs and description sentences, the two failure codes on three tools, the `app` hint, the `trellis_endpoints` entry and `toolVersion` 24. The schemas stay self-contained, with local `$ref`s only and no `$schema` (#836). The modern `tools/list` frame is 232,468 bytes (legacy 232,411), 11,198 bytes more than at `toolVersion` 23, against the 235,520-byte compacted-frame limit (owner ruling 2026-11-09, #851) and the unchanged 245,760-byte named headroom limit. `trellis_endpoints` is 9,363 compact bytes.
- **Choices beyond the issue text.** The name-or-manifest `app` value naming every same-named app; check filtering rows only, with whole counts; the 500-name find scan; the call graph not checking its root; the `building`/`invalidRules` describe states instead of waiting; the per-row call-site samples (5, or 50 in the single-endpoint form); exact `method` and `pathTemplate` matching; and extraction without a rules file.

## Amendment (#855): the proposal prompt's closed grammar accepts `interfaces`

**Status: owner-ratified 2026-11-10 ([#855](https://github.com/SquashMerge/trellis/issues/855)).** The owner ruled that JavaScript and TypeScript request wrappers are configured in `.trellis/rules.toml` rather than in each client's `package.json`. `toolVersion` is **25** (previously 24, #851). The bump records the prompt text change below.

- **Prompt.** The proposal prompt's closed TOML grammar lists `interfaces` among the accepted top-level keys. One sentence says that optional `[[interfaces.wrappers]]` entries `{callee,method,at,prefix,root|app}` name request wrappers for HTTP interface evidence only and never change verdicts. The table's rules are in [architecture-view.md](architecture-view.md#apps-and-http-interfaces-data-and-extraction-784-788).
- **Catalog.** Only the `toolVersion` constant changes, from 24 to 25, which has the same width. The prompt text is not part of the tool catalog, so the `tools/list` frame size is unchanged from #851's: 232,468 bytes modern and 232,411 legacy, against the 235,520-byte compacted-frame limit.

## Tool catalog

### `trellis_workspace_describe` (W1)

Input exactly `{schemaVersion:1,workspace?:string}`; no pins or indexing flags. Data exactly `{workspaceLabel:Text,indexState:"indexing"|"reconciling"|"ready"|"unavailable",progress:{completed:UInt,total:UInt|null}|null,watcherDegraded:boolean,languages:LanguageStatus[],toolVersion:7,schemaVersion:1,limits:Limits,searchIndex:SearchAvailability}`. `SearchAvailability={backend:"cpu"|"metal"|null,unavailableReason:null|"model_not_configured"|"model_load_failed",configuration?:SearchConfiguration}` is daemon-wide diagnostics, not query-pinned readiness or coverage. The same diagnostics appear under `searchIndex` in CLI status; `SearchConfiguration` follows the [#594 amendment](#amendment-594-user-model-configuration-diagnostics). A missing model configuration reports `model_not_configured`; verification/loading failures report `model_load_failed` until a successful load clears the reason. No failure is inferred from an untried, loading or idle model. Standalone registries without a search service have null backend and reason because configuration is unknown. Label is not an absolute path. `completed<=total` when total known; progress null if ready, unavailable or unmeasured. `LanguageStatus={language:Language,nativeCoverageTier:"syntax",coverage:{selected:UInt,complete:UInt,partial:UInt,failed:UInt,unselected:UInt},semantic:SemanticStatus}` (`semantic` per [#528](#amendment-528-imported-evidence-and-agent-sized-responses)); `coverage` counts native coverage tuples, **not** universal language capabilities, and complete+partial+failed=selected. When ready return exactly five languages in #22 order (java, rust, python, javascript, typescript), even with zero counts. Otherwise `languages:[]`, null basis, no previous coverage advertised as current. Describe warnings=[], truncated=false, truncationReason=null; partial iff not ready or any selected partial/failed count is nonzero.

### `trellis_find_symbols` (W2)

Input `{schemaVersion:1,workspace?:string,query:string,limit?:UInt,expectedBasis?:Basis}`. Query is 1–256 UTF-8 bytes, literal name/ID substring: no trimming, regex, glob, wildcard or Unicode normalization. Use SQLite built-in ASCII `lower` and literal `instr`. Default limit 20; accepted 1–50. Search all admitted source sets/documents at one eligible snapshot, with no path/language/source-set filter. Rank exact case-folded name, name prefix, other name/ID substring; ID-only is rank three. Within each match tier, rank stored `production` declarations before `test` declarations ([#594](https://github.com/SquashMerge/trellis/issues/594)); exact test names still precede production prefixes. Then tie-break original name (null first), then ID in UTF-8/ASCII byte order. Fetch one extra match to detect omitted rows. [Decision 0013](semantic-evidence/decisions/0013-symbol-search-trigram-prefilter.md) finds candidates through a trigram prefilter and verifies each with this rule; the output is unchanged. Data exactly `{symbols:Symbol[],documents:DocumentRef[],coverage:Coverage[],scopeCoverage:LanguageStatus[],provenance:Provenance[]}` (amended by #156, owner-ratified 2026-10-16). No snippets or bodies.

- **`scopeCoverage`** reuses describe's `LanguageStatus`: the rows the [#715 result-language rule](#amendment-715-compact-find-and-search-result-language-scope-rows) selects, in #22 order (java, rust, python, javascript, typescript), including zero counts. Counts come from **all native coverage tuples** in the same eligible find snapshot, across every searched source set, independent of query, limit and response-byte clipping. `selected=complete+partial+failed`; `unselected` counts notRequested, omitted and unsupported tuples; the total is `selected+unselected`, with no separate field. These are tuple counts, not symbol counts or universal language support. Never join a separately sampled describe result.
- **`coverage`** is the sorted, deduplicated native coverage closure of the **emitted** symbols only, keyed by full `DocumentKey`, revision and producer. It excludes the limit+1 lookahead and any symbol removed for response bytes. **`provenance`** is the complete closure for those emitted symbols. Zero emitted symbols means `documents`, `coverage` and `provenance` are all empty.
- **Completeness:** any selected partial or failed tuple anywhere in the searched scope (from `scopeCoverage`) makes find `partial:true` and adds one aggregate `coverageIncomplete` warning before `syntaxOnly`, even on zero hits or with healthy hits elsewhere. Truncation also makes it partial. Unselected-only tuples do neither, but `unselected>0` and a zero-hit result never prove universal absence. This warnings-v1 aggregate projection, used by find and search, does not change declaration, outgoing_calls, read_source or #22 graph semantics.
- No hit is `symbols:[],documents:[]`, **not** not_found. Empty symbols cannot prove complete extraction.
- **Bounds:** the 65,536-byte cap applies to the whole structured-content-plus-summary JSON-RPC line, including LF and its scope rows. Emit the longest fitting ordered symbol prefix and recompute its coverage, provenance and warnings. If the first hit cannot fit but the empty prefix does, return `symbols:[],documents:[]`, `truncated:true`, `truncationReason:"response_bytes"` (not a false negative). Only a fixed empty-prefix envelope that cannot fit is `range_too_large`. No pagination or path/kind filter.

### `trellis_search_symbols` (#211)

Input exactly `{schemaVersion:1,workspace?:string,query:string,role?:"any"|"production"|"test",limit?:UInt,expectedBasis?:Basis}`.
`query` is 1–1024 UTF-8 bytes, with no trimming or normalization; limit defaults to 10 and
accepts 1–50. `role` defaults to `"any"`, retaining both stored declaration roles and the ranking
below. `"production"` or `"test"` searches only declarations with that stored measured role,
without path guessing. Apply it to both arms **before ranking and each depth cutoff**, not to
the output prefix, so a filtered search can return up to `limit` hits of that role. An invalid
role value, including explicit null, is typed `invalid_request`. No other filter, score/debug
flag, backend selector, cursor, indexing flag or `view` is accepted. Backend configuration is
not a new tool input.

Data exactly `{role:"any"|"production"|"test",hits:SearchHit[],documents:DocumentRef[],coverage:Coverage[],
scopeCoverage:LanguageStatus[],provenance:Provenance[],searchIndex:SearchIndexStatus}`.
`data.role` always echoes the effective role, including the default `"any"` on omission.
`SearchHit` is exactly `Symbol` plus `{rank:UInt,matchedBy:("lexical"|"vector")[]}`.
Rank is 1-based and contiguous in the emitted ordered list. `matchedBy` is nonempty, unique,
and ordered `lexical` before `vector`; it records which bounded ranking arms returned the hit.
There is no numeric relevance score or relevance provenance. `displayKey` stays output-only:
it never enters card hashes, lookup, ranking, ordering or joins.

`SearchIndexStatus` is exactly:
`{state:"complete"|"building"|"partial"|"unavailable",modelId:Hash|null,
modelName:Text|null,backend:"cpu"|"metal"|null,declarations:UInt,embedded:UInt,
lexical:"complete"|"unavailable",unavailableReason:null|"model_not_configured"|"model_load_failed"|"vector_budget_exceeded"|"model_not_ready"|"matrix_not_resident"}`.
`declarations` counts eligible candidate declarations at the admitted head; `embedded` counts
those with a current decoded vector for the active model and satisfies
`embedded<=declarations`. These are admitted-head counts, independent of the role filter;
`scopeCoverage` also remains unfiltered. No configured/verified/ready model, or no matching admitted
resident matrix (including first search, eviction, head-change races or an oversized head),
gives a lexical-only answer with null `modelId` and null `modelName`. Its state is `building`
while the background owner works on this head (below), otherwise `unavailable` with
`embedded:0`, an unavailable sentinel rather than a measured vector count.
**Amendment (#777, owner-ratified 2026-11-07):** `toolVersion` is **14** (previously 13, #769).
`unavailable` always has a non-null `unavailableReason`; there is no
reasonless `unavailable`. The reason is `model_not_configured` when no model path is configured
(including a standalone registry with no search service), or `model_load_failed` after a
verification/loading failure, cleared by a successful load. Otherwise it is `model_not_ready`
when this query got no query vector (the model was loading for another job, or query inference
failed), or `matrix_not_resident` when there is no matching resident matrix and the owner has
no work for this head (memory policy, an eviction no request has rearmed, or a head race). Model
diagnostics and `vector_budget_exceeded` take precedence over `building`. Backend selection is independent: report the
backend actually selected whenever known, including while loading/building or when the model
is unavailable; `backend:null` means only that no backend has been selected. With a ready
model and an admitted matching resident matrix, `complete` means
all candidates have vectors, `building` means vectors are missing and a background job is
running, and `partial` means vectors are missing and no job is running. Without a used matrix,
the state is `building` while the owner has queued, running or deferred work for this checkout's
head. That covers right after a publication, before the new head's matrix exists, and work
deferred until reconciliation ends. A no-op hint for an evicted head, or for the same head the
memory policy declined (same declaration count at the current cache epoch), does not count; a
different, e.g. smaller, successor head is pending again. Retry exhaustion settles the head until
the next hint. The hits are then lexical-only, model identity is null, and `embedded` counts the
admitted candidates with a current decoded vector in one vector-cache snapshot. Both the hash
enumeration and that count are optional work under the vector cutoff: past it, search returns
the complete lexical answer with `vector_budget_exceeded`. The count may equal `declarations`. Missing or evicted
vectors do not change native coverage or evidence completeness. A candidate without a vector
can still be returned by the lexical arm. If this call exhausts its optional vector budget,
that query-specific reason takes precedence over model diagnostics. Then the complete vector arm is discarded, state is
`unavailable`, model identity is null and `embedded:0` is an unavailable sentinel, not an
observed cache count. `declarations` remains the exact eligible head count and backend remains
the selected backend. The reason is `vector_budget_exceeded`, all returned hits are lexical,
and lexical remains complete. This reason changes no native coverage, warning, truncation or
partial-result meaning.

Successful search has `lexical:"complete"`. A required head search table that cannot be read
is a store failure, not a lexical-unavailable success. A query that produces no lexical terms
skips FTS MATCH but still admits eligible exact names; its lexical arm is empty only when no
such names exist. Term-free queries retain mandatory head/corpus checks and typed store failures.

Search uses only the validated head admitted under W8. `expectedBasis` is optional. A complete
pin unequal to that admitted head returns `revision_conflict`; retained older revisions are
not eligible. Candidates, card hashes, lexical scores, documents, coverage and provenance all
come from that one index snapshot. A publication during the call never moves the answer.
Follow-up declaration/call reads at the returned basis can conflict after a new head; search
does not promise continued verification at an older basis.

Documents, coverage and provenance close over the emitted hits only, exactly as W2. Zero
emitted hits gives three empty arrays. `scopeCoverage` follows W2’s (#715) rows and counts
all native tuples in the searched snapshot, independent of matching, limit and byte clipping.
Selected partial/failed extraction anywhere in that scope sets `partial:true` and the aggregate
`coverageIncomplete/null` before `syntaxOnly/null`, including on zero hits. Missing mandatory
proof, corrupt digests or mixed revisions fails `store_unavailable`, without partial evidence.

Use the ranking rules in
[Decision 0018](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md#lexical-query-and-ranking).
Exact is stored measured `Declaration.name` equal to the complete raw query byte-for-byte:
no trimming, case folding, normalization, displayKey parsing or FTS stemming. Before lexical
depth D, add eligible exact names to ordinary FTS matches, including term-free names and
names otherwise below D, and deduplicate by (document version, symbolId). This is additive,
not an exact-only gate. Lexical order is exact first, production before test, then scored
candidates by BM25 ascending before unscored exact-only candidates within each exact/role
tier, then symbolId bytes. There is no kind tier. This also governs lexical-only search:
a weak production match can precede a stronger test match in the same exact/nonexact tier.

The fixed weighted reciprocal rank fusion constants remain (k, D, wL, wV) = (20, 200, 1, 4).
After ordinary RRF, exact names precede all nonexact candidates; within exact names,
production precedes test, then the existing RRF score/rank/symbolId tie rules apply.
Nonexact hybrid hits keep the existing RRF order, without a final production tier.
At most D candidates per arm survive; `matchedBy` is membership after each cutoff, and
additionally admitted exact names are lexical members. There is no new score field or
change to tokenizer, card/model identity, storage schema, publication or trigram lookup.
These are owner-reviewed choices, not runtime tuning. Under the
[resident-matrix amendment](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6441405819),
the vector arm and search-index counts use one immutable published int8 matrix snapshot for
the admitted index pin and active model, together with the same mandatory index snapshot.
The [#358 owner memory/startup policy](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6446057555)
sets a 2 GiB aggregate matrix-allocation cap, including in-progress builds, replacements and
staging buffers, using conservative estimated allocation capacity and bookkeeping before
allocation. It is not an exact process-RSS cap and excludes model weights. Query use shares
the owner's existing matrix charge, not a second copy. Normal work uses LRU eviction of
eligible matrices. Each matrix also expires 600 seconds after its last query use or publication;
background fill does not reset its timer. A running head is excluded from idle eviction until
its job ends. Startup warming selects at most the four
most recently opened checkouts within seven days (newest first, root-key byte-order ties),
using existing open-age metadata read-only without activation, metadata writes or a durable
MRU catalog. It is low-priority background work that never blocks daemon socket acceptance,
and stops at cap admission failure without evicting matrices for more startup work.

Other checkouts return complete lexical results on first search while the independent
background owner builds their matrices. A single head that cannot fit remains lexical-only.
When no matching matrix is retained or acquisition races with a head change, search also
returns lexical-only with null model identity and the selected backend: `building` with the
cached count while the owner works on this head, else `unavailable`, embedded:0 and
`matrix_not_resident` (#777). It never waits for the owner or embeds a query without
an admitted resident matrix; the streaming scorer cannot bypass the cap. This supersedes
the earlier missing-matrix streaming fallback. Exact admitted declaration counts and lexical
retrieval remain mandatory, and observed store faults remain typed failures rather than
lexical-only success. Counts and ranks never mix snapshots. Neither cache state nor matrix
publication is pinned by `evidenceBasis`.
A valid retained published matrix remains usable after backing-cache eviction. Owner updates,
residency changes, filling and re-embedding can change rankings and counts between calls at
the same basis. There is no cross-call or bit-parity promise.

Emit the longest ordered hit prefix fitting the complete 65,536-byte response line, including
summary, LF and fixed scope/status overhead. Recompute emitted-only documents, closure and
warnings for each prefix. More eligible hits than limit gives reason `limit` when all limited
hits fit; any limited hit dropped for bytes gives reason `response_bytes`, which takes
precedence. The first hit not fitting yields an empty truncated prefix if the fixed envelope
fits; only an empty envelope that cannot fit fails `range_too_large`. Truncation sets partial
but adds no warning. There is no pagination. No hits is a successful empty list, not not_found
and not proof of absence.

The existing five-second acceptance-to-decision deadline and cancellation/root checks apply,
including query inference and cold-load handling. Optional vector work has one absolute
3,000ms sub-budget from acceptance, leaving 2,000ms for cleanup and mandatory lexical work.
It covers model admission, verification/read, loading, embedding, cache access and scoring;
it never resets on retry. Verified model weights are reused by the single owner with the
[ten-minute idle timeout](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md#model-identity-and-runtime),
reset by query/fill use. Each resident matrix instead expires 600 seconds after its last query
use or publication, not background fill; a running head is excluded from idle eviction until
its job ends. Loading remains lexical-only. On that budget alone, cooperatively stop query vector work, discard the
whole vector arm and return the complete lexical result with the reason above. Query model
inference is never detached. An independent matrix build remains with the single background
fill owner and finishes/publishes after vector-budget fallback or query cancellation/drop;
it is not query inference or a read-side cache write. Client
cancellation and mandatory deadline/root checks still apply. Mandatory index/FTS contention
remains typed storage_busy, retried within the call deadline, then deadline_exceeded. Optional
cache contention is also typed/retried, but expiry of its vector budget permits this reasoned
fallback; BUSY itself is not permission for lexical-only success or evidence of corruption.
Database-level vector-cache failures, including corruption, schema mismatch and unreadable or
unsafe storage, fail store_unavailable. A vector-cache row that fails typed decoding is a
specified miss under T00/T02, excluded from the selected cache snapshot's embedded count and
re-embedded outside the read. This does not invalidate a valid retained published matrix.
A cache file the owner is still creating (no version and no schema yet) reads as an absent
cache, not a schema mismatch (#777).

**Amendment (#777, owner-ratified 2026-11-07): head coverage in describe and CLI status.** Workspace describe and CLI
status `searchIndex` (`SearchAvailability`) add optional `state:"complete"|"building"|"partial"|"unavailable"`,
`declarations:UInt`, `embedded:UInt|null` and `cacheDiagnostic?:"vector_cache_unavailable"|"vector_cache_busy"`:
this checkout's coverage at the admitted head. Describe and status never fail on the optional
vector cache: if the cached count cannot be read while the owner is pending (a corrupt,
unreadable or unsafe cache, or cache contention), they report `building` with `embedded:null`
and that typed `cacheDiagnostic`, present only then. Search keeps its typed `store_unavailable`
for database-level cache failures. Its
precedence follows search without the per-query causes. A model diagnostic gives `unavailable`
with that reason. Otherwise the matrix the owner published for that pin gives `complete`, or
`building` while the owner runs for that pin, else `partial`. Otherwise owner work for the head
gives `building` with the cached count, and no owner work gives `unavailable` with
`matrix_not_resident`. With these fields, `unavailableReason` is the state's reason, so it can
also be `matrix_not_resident`, and it is non-null exactly when the state is `unavailable`. The
fields are omitted without a search service or matrix owner (a standalone registry keeps
`{backend:null,unavailableReason:null}`) and without an admitted head. The other describe
`SearchAvailability` text stands.

The tool description states: “Find candidate declarations by lexical and local vector
retrieval. Relevance is not evidence. Inspect hits with trellis_declaration at the returned
basis; a later publication can make that basis conflict. Zero hits does not prove absence.”
Search relevance never becomes #22 evidence, a binding, durable-anchor replacement or an
input proof for another tool. No hosted embedding or reranker is in scope.

### `trellis_declaration` (W3)

Input exactly `{schemaVersion:1,workspace?:string,symbolId:SyntaxId,expectedBasis?:Basis}`. `limit`, `view`, `depth`, graph budgets and all other extra fields fail `invalid_request`, not an empty projection. IDs retain an 8192-byte ceiling and must have canonical #22 syntax. Invalid syntax gives `invalid_request`; absent valid ID gives `not_found`.

Declaration data exactly `{declaration:WireDeclaration,displayKey:Text,doc:UInt,lines:Lines,evidence:Evidence,documents:DocumentRef[],coverage:Coverage[],provenance:Provenance[]}`. `WireDeclaration` projects the complete selected #22 `Declaration` **except** `document`, `revisionId` and `provenanceId`, supplied instead by the data-level `doc` entry; it retains `syntaxId,kind,name,lookupKey,role,ancestors,key,range,nameRange,header` (including required `role:"test"|"production"`, distinct from evidence `Role`) and their nested #22 shapes and measured array order. Outer `contentHash` is supplied by that same entry. Its `displayKey` is the qualified label of the selected measured declaration, even when its name is null; it is presentation-only. `lines` covers the declaration's `range`. Do not add source body, guessed type or durable-anchor schema. A body-only edit can keep syntax ID yet change hash/basis; a complete old pin conflicts rather than silently selecting another revision. This is one complete object: if it does not fit, return `range_too_large`, not a shortened declaration.

### `trellis_outgoing_calls` (W4)

Input exactly `{schemaVersion:1,workspace?:string,symbolId:SyntaxId,limit?:UInt,expectedBasis?:Basis}`; optional `limit` defaults to 6 (#528) and accepts 1–50. `view`, `depth`, graph budgets and all other extra fields fail `invalid_request`. IDs use W3's canonical syntax, 8192-byte ceiling and invalid/missing-ID behavior. The retained W2 find `limit` is independent; only this tool of the **new W3/W4 pair** accepts `limit`.

Outgoing data exactly `{symbolId:SyntaxId,displayKey:Text,calls:CallItem[],documents:DocumentRef[],coverage:Coverage[],provenance:Provenance[]}`. The selected owner’s `displayKey` is its qualified measured-declaration label. `CallItem` exactly `{callId:OccurrenceId,ownerSyntaxId:SyntaxId,ordinal:UInt,doc:UInt,range:Range,calleeRange:Range|null,lines:Lines,calleeText:string|null,calleeTextTruncated:boolean,displayKey:Text,nativeCandidates:NativeCandidates,binding:CallBinding|null,disposition:"resolved"|"external"|"ambiguous"|"unresolved"|"unsupported"|"dynamic"|"declarationOnly"|"staleTarget",targetId:SyntaxId|null,boundaryReason:"missingEvidence",...Evidence}`. First release fixes binding=null, disposition=unresolved, targetId=null and boundaryReason=missingEvidence even when nativeCandidates has items: lexical hints are **not** producer-established #22 bindings. Emit each measured call whose owner is the selected declaration once; exclude nested callable bodies. Depth-one measured sites, not graph traversal. Order by `(document.path,range.start,range.end,callId)`. Complete relevant coverage and no measured calls allow an empty complete answer; incomplete coverage cannot support that claim. Read calleeText only from cached calleeRange; absent range means null/false. Clip at 1024 UTF-8 bytes on a scalar boundary and flag clipping; never export unbounded native spelling or arbitrary body/invocation text. `CallItem.displayKey` is `ownerDisplayKey + "@byte:" + decimal(call.range.start)` plus `":" + bounded calleeText` only when calleeText is non-null; absent text still yields the owner/site label. `ownerDisplayKey` is the selected measured declaration label above, not a target. For `src/a/Invoice.java#Invoice.total`, start byte 412 and bounded calleeText `foo`, the call label is `src/a/Invoice.java#Invoice.total@byte:412:foo`; it asserts no resolved callee. Every `displayKey` is result-only presentation text: never accept one in a request or use it for identity, hashes, ordering, joins, resolution, or completeness.

The [#22 advisory-native-candidates rule](semantic-evidence/contract-v1.md#advisory-native-candidates-for-the-first-mcp-call-view-owner-decision-2026-09-25) permits a hint only when full measured `Call.spelling` and `calleeRange` prove **one** language-decoded identifier token, not a compound expression. A member call `obj.foo()` may use measured member-name `foo` only when its `calleeRange` and full `Call.spelling` themselves verify that one identifier token; never strip `obj.` from compound spelling `obj.foo` or infer a token from the invocation. Match the verified token’s decoded lookup key only to an already indexed fresh native declaration with equal language-specific `lookupKey` in the **same eligible response snapshot**, language and source set; `expectedBasis` remains optional. Project that declaration's actual `syntaxId`, `name`, `range` and `doc` referring to its verified document, revision, hash and native provenance; verify the canonical ID rather than guessing one from a name, legacy node ID, span or SCIP symbol. Do not infer receiver, import, scope, overload or dispatch. A hint with unprovable token, ID, eligibility or optional native provenance/coverage is omitted, and an empty list proves no negative callee claim. A missing or corrupt **required** selected call/declaration coverage or provenance, mixed revision or digest mismatch instead fails `store_unavailable`, never a false-green empty hint list. The returned coverage/provenance arrays include required native rows for both each emitted call and each shown declaration; the name-match relation itself has no producer provenance and borrows none as semantic proof. Candidate presence, absence or clipping adds no warning beyond warnings-v1. Deduplicate verified hints by `syntaxId`, rejecting conflicting duplicate records as invalid store evidence. **Test code (#167; [Decision 0011](semantic-evidence/decisions/0011-test-production-declaration-role.md)):** when the selected caller declaration has stored `role:"production"`, a matching declaration with stored `role:"test"` is not eligible, including inline tests. Skip it after loading its typed role and before optional candidate verification; never count it in `omitted`. The producer alone uses #167 test-convention paths to assign roles: a `tests`, `test`, `__tests__` or `spec` directory segment; Rust `tests.rs`, `*_tests.rs` or `*_test.rs`; Python `test_*.py`, `*_test.py` or `conftest.py`; Java `*Test.java` or `*Tests.java`; and `*.test.*` or `*.spec.*` file names. Do not recompute role from paths during this read. **Order (#167):** sort by proximity to the calling document (the same document, then the same directory, then elsewhere), then by `(document.path,range.start,range.end,syntaxId)` with #22 byte ordering; show at most the first eight per emitted call. **Rank first (#170):** rank the eligible lexical rows by this order using their indexed path and range, then verify them in rank order only until eight hints pass; a row that fails verification is skipped and the next one is tried, and rows after the eighth verified hint are left unexamined. `omitted` when numeric counts, for that emitted call, the verified hints not shown because of the response-byte cap plus the distinct eligible matches left unexamined after eight hints passed (#170); a match that was examined and failed verification is never counted. Optional hint matching/count gets at most `hintWorkMs:1000` **total across all emitted calls** within (never extending) the overall five-second call deadline. If that optional work cannot finish in its budget, discard any partial hints for the affected call and return `items:[],omitted:null,complete:false` for it and every unprocessed emitted call; preserve already completed calls and the mandatory call answer: do **not** fail it, claim zero, or mark top-level truncation solely because hint count is unknown. An actual mandatory call/read deadline remains `deadline_exceeded`, and corrupt required selected evidence remains `store_unavailable`. The false `complete` value is invariant even when every indexed lexical match fits: it says nothing about possible callees. `nativeCandidates` never sets targetId, disposition, binding, a graph edge or traversal authority; #40 uses only admitted exact semantic bindings. #22 [resolution cardinalities and expansion](semantic-evidence/contract-v1.md#dispatch) remain authoritative.

**Owner amendment (2026-10-23, #194 item 3 option (a); native-v9):** Rust may measure the final segment of a `scoped_identifier` callee as the single verified token, using only the parser's `name` field when it is a lone `identifier`. Unwrap a parser-recognized `generic_function` via its `function` field first. Thus `Self::foo()`, `module::foo()`, `Type::foo()`, `Type::<T>::foo()` and `Type::foo::<T>()` can expose exactly `foo` as `Call.spelling` / `calleeRange`. This eligibility applies to outgoing advisory hints and incoming name matching. It is not path resolution and never suffix-splits source text or a compound expression. A final segment not proved to be an identifier yields no verified token. Keep the fixed limitations tuple and all wire shapes unchanged. Tests cover each form and a parser-proved non-identifier negative. This records the existing [owner ruling](https://github.com/SquashMerge/trellis/issues/194#issuecomment-6375543246), not a new ratification request or the unrelated #194 read/publication optimizations.

**Amendment (#467): published semantic bindings.** Both `trellis_outgoing_calls` and `trellis_incoming_calls` also accept an optional `semanticProducerId:string` of 1–512 UTF-8 bytes. An empty, oversized or non-string value, explicit null, or a NUL fails `invalid_request`. A well-formed ID that the admitted revision does not publish fails `not_found` with `Semantic producer not published for the admitted revision`, as `trellis_call_graph` does. Without the field, the per-language default applies at the admitted head (owner ruling 2026-10-30, #483, amending #392/#409): each call uses the sole published producer whose profile covers its document's language. A language that none or several cover selects nothing, and producers are never merged. An explicit producer applies to the languages it covers. When a call's language has a selected producer, its emitted item carries the call's binding. The binding is selected by the reader that the call graph, sequence and line navigation share (#466). `binding` is the published `CallBinding`: resolution, declared target, dispatch, and the whole candidate union for an ambiguous call. `disposition` is its resolution, or `staleTarget` for a stale boundary. `boundaryReason` is the call graph's reason (`none` means the graph would traverse the call; `dispatch` means dispatch is not proven direct). `targetId` is a resolved binding's declared internal target when it is located exactly in the admitted snapshot and is not stale. `evidenceTier` is `semantic`, and `semanticBasis` and `freshness` come from the binding's provenance. A call with no binding keeps the syntax-only values above. The data's `coverage` and `provenance` add the producer's coverage tuple and every proof of each emitted binding. Warnings follow warnings-v1: `coverageIncomplete`, `staleEvidence`, `staleTarget`, `bindingAmbiguous` for an emitted ambiguous binding, and no `syntaxOnly`. An item whose language has no selected producer keeps the syntax-only values. `syntaxOnly` then carries the reason of each such language of the emitted calls (the selected declaration's language when there are none), joined by `; `: `no semantic producer published; showing syntax`, `multiple semantic producers for <language>; showing syntax` or `semantic producer does not cover this language; showing syntax`. `partial` counts any emitted call whose `boundaryReason` is not `none` as a boundary. Incoming callers are still found by name (`matchedBy:"name"`); the binding annotates each found call and adds no callers. The [#724 owner ruling](#amendment-724-callers-bound-elsewhere-and-bounded-graph-spellings) also removes callers proven bound elsewhere.

### Incoming calls (syntax tier)

[Decision 0012](semantic-evidence/decisions/0012-incoming-calls-syntax-tier.md) adds the standalone sixth tool `trellis_incoming_calls`, **not** an inspect view or a W6 protocol revision. Input exactly `{schemaVersion:1,workspace?:string,symbolId:SyntaxId,limit?:UInt,expectedBasis?:Basis}`. The limit defaults to 6 (#528) and accepts integers 1–50; an out-of-range integer fails `range_too_large`. W3's canonical ID syntax and 8192-byte guard, verified workspace selection and optional complete basis pin apply. Invalid syntax, unknown fields, `view`, semantic producer selectors and explicit null optional fields fail `invalid_request`; a well-formed ID absent at the selected head fails `not_found`. An old complete basis conflicts before ID lookup. No selector or typed semantic binding field is reserved here.

Resolved success uses the shared closed envelope with `data` **exactly** `{symbolId,displayKey,match,callers,documents,coverage,scopeCoverage,provenance}`. `displayKey` is the selected declaration's qualified presentation label, not a target. `match:NameMatch` is **exactly** `{basis:"name",sameNameDeclarations:UInt,limitations:["aliases","functionValues","reExports","dynamicDispatch","opaqueMacros","sameNameDeclarations"]}`, with that fixed ordered limitations tuple. The count is of all eligible selected native declarations sharing the selected `lookupKey` in the same source set and language, including the selected declaration, regardless of caller role, returned call count or clipping. A selected anonymous declaration with null key yields an empty name match and count zero, without inventing a key.

Each `IncomingItem` is **exactly** `{callId:OccurrenceId,ownerSyntaxId:SyntaxId,callerDisplayKey:Text,callerRole:"production"|"test",doc:UInt,lines:Lines,range:Range,calleeRange:Range,calleeText:string,matchedBy:"name",binding:null,disposition:"unresolved",targetId:null,boundaryReason:"missingEvidence",evidenceTier:"syntax",semanticBasis:null,freshness:"fresh",staleBecause:[]}`. The owner is the measured caller declaration (including the selected declaration for recursion); `callerDisplayKey` follows the existing measured qualified-declaration formula, not a target label. The role is the caller declaration's **stored** Decision 0011 role, never inferred from path at read time. `range` is the measured call site; `calleeRange` is its non-null measured token range; `calleeText` is the **complete** token in selected captured bytes, not the invocation, a suffix or clipped text. There is no `calleeTextTruncated`, native-candidate hint, optional-match list or cursor in this schema. Site `doc` and `lines` use the shared selected-version DocumentRef and LF-line rules. Each item has fixed null binding and target, missing-evidence boundary and fresh syntax evidence: neither name equality nor candidate ambiguity establishes semantic binding, dispatch or a graph edge.

A caller qualifies only when `Call.spelling` is the complete measured identifier token, its parser-recognized `calleeRange` covers **exactly** those bytes in the selected captured source, and the supported language-decoded lookup key equals the selected declaration's key in the same eligible snapshot, language and source set. A measured member token in `obj.foo()` can qualify when the full measured spelling is `foo`; a compound spelling `obj.foo` cannot be suffix-split or guessed. The #194(a) Rust scoped/generic callee rule above likewise requires the parser-proved final identifier, not a suffix guessed from a path string. Rust raw `r#` identifiers compare by decoded name; Python uses NFKC; Java, JavaScript and TypeScript use decoded code points (JavaScript and TypeScript `\uXXXX`/`\u{...}` only where the parser measured a valid single identifier). Java backslash-escaped identifier syntax is not established by the current parser, so unverified forms cannot match. This is a name search, not a semantic references query; it misses aliases, function values, re-exports, dynamic dispatch and opaque Rust macro calls and may include calls to another same-named declaration.

Include test callers and self-recursion. Sort by `(callerRole production before test, proximity to selected declaration document: same document then same directory then elsewhere, caller document path bytes, call range.start, call range.end, callId)`. Search only selected revision documents in the declaration's source set and language with selected complete or partial native coverage; no cross-revision or fallback evidence. `scopeCoverage:LanguageStatus[]` always has five rows in #22 order. It counts **all native coverage tuples in that searched source set and language** at the pinned head, independent of matching name, limit and byte clipping; the other four rows have zero counts. `selected=complete+partial+failed`; `unselected` counts notRequested/omitted/unsupported. A failed or unselected tuple is counted but never fabricated as a caller. `documents` contains exactly the emitted caller call-site versions, sorted and ref-numbered under the shared rule; a selected declaration present only in mandatory closure adds no document ref to an empty list. `coverage` and `provenance` contain the sorted, deduplicated **required** native closure of the selected declaration and emitted calls and their measured caller declarations, not omitted callers or a borrowed name-match proof. Missing/corrupt required selected rows, source proof, coverage or provenance, invalid typed rows or mixed revisions fail `store_unavailable`, not an empty result. Keep selected-document read attestation, but do not remeasure live source or impose new per-field read-side caps.

`partial:true` iff the required caller list is truncated, a selected partial/failed native tuple occurs in the searched scope, or an emitted unresolved boundary call exists. Even if an empty result has selected complete coverage and `partial:false`, its fixed `match` basis and limitations prevent a claim of no callers. Always emit `syntaxOnly` with null provenance; add `coverageIncomplete` with null provenance iff a searched-scope selected tuple is partial/failed, in warnings-v1 order. No new warning code for name matches, ambiguity, absence, or truncation. The full `scopeCoverage`, match and selected declaration closure remain present when `callers:[]`.

The caller list is **required**, not optional hints. After sorting, consider at most limit plus one eligible item for limit detection; emit the longest prefix that fits the complete 65,536-byte JSON-RPC response line including structured content, summary and LF. Recompute emitted-only document refs and mandatory selected-owner-plus-caller coverage/provenance closure for each prefix; never trim proof or scope coverage to make room. More eligible items than `limit` with every limited item fitting sets `truncated:true,truncationReason:"limit"`; dropping any limited required caller to fit sets `truncated:true,truncationReason:"response_bytes"`, which outranks `limit`. Truncation sets `partial:true`, but adds no warning. If the fixed empty-prefix envelope cannot fit, or the first required item with its complete token and proof closure cannot fit, fail `range_too_large`, not a falsely green empty list. The five-second deadline starts at protocol acceptance and includes validation, read admission/materialization and send decision; existing cancellation and typed-error precedence apply. A success summary uses `trellis_incoming_calls: N calls[, truncated: <reason>]; full result in structuredContent.` with singular `call` for one.

When #59's exact semantic tier is implemented it amends **this same tool's** closed input and output with `matchedBy:"binding"` and a semantic match basis. #59's #40/#41/#58 prerequisites remain required for that later tier. The #467 amendment above already accepts `semanticProducerId` and fills each name-matched caller's binding fields; the "semantic producer selectors" rejection and fixed `binding:null` values in this section apply only when no producer is selected.

### `trellis_call_graph` (#40)

A separate tool for the rooted graph capability ([Decision 0012](semantic-evidence/decisions/0012-incoming-calls-syntax-tier.md): one tool per capability). `trellis_outgoing_calls` is unchanged. It is entry 8 of the catalog, after `trellis_read_source` and before the architecture tools.

**Input**, closed: `{schemaVersion:1,workspace?:string,symbolId:SyntaxId,semanticProducerId?:string|null,depth?:UInt,maxNodes?:UInt,maxCalls?:UInt,includeExternal?:boolean,includeTests?:boolean,expectedBasis?:Basis}`. `depth` is 0-5 (default 2), `maxNodes` 1-150 (default 150), `maxCalls` 0-500 emitted edge groups (default 500). `includeExternal` and `includeTests` are closed booleans, both default false; null or any non-boolean is invalid. Defaults materialize in GraphRequest. This follows the [owner-ratified #379 trimmed amendment](https://github.com/SquashMerge/trellis/issues/379#issuecomment-6480360842) to [both graph contracts](semantic-evidence/contract-v1.md#rooted-graph-answers). `semanticProducerId` omitted or null applies the per-language default (owner ruling 2026-10-30, #483, amending the syntax-only default): each node's document uses the sole published producer whose profile covers its language, so a graph that crosses languages uses each language's producer. A language that none or several cover is syntax only, and `syntaxOnly` names each returned such language's reason, for example `multiple semantic producers for rust; showing syntax`. An empty string is invalid. There is no `revisionId` or `sourceSetId` input. Out-of-range, non-integer, null or unknown fields are `invalid_request`: bounds are never clamped and are never `range_too_large`. Validation precedes capacity and read admission ([Decision 0009](semantic-evidence/decisions/0009-checkout-worker.md) section 7).

**Basis.** The server admits the head once. `expectedBasis` must equal the admitted pin, otherwise the call is `revision_conflict`; a retained older pin is never served. The server fills `sourceSetId` and `revisionId` of the contract `GraphRequest` from the admitted native revision, so the echoed `evidenceBasis`, `data.requested.revisionId` and `data.result.resolvedRevisionId` agree. There is no current-revision fallback.

**Success envelope**, closed: `{schemaVersion:1,requestId,workspace,catchingUp,evidenceBasis,data:{requested:GraphRequest,result:GraphResult},warnings,partial,truncated,truncationReason}`. `GraphRequest` and `GraphResult` are exactly the [contract records](semantic-evidence/contract-v1.md); `warnings`, `partial` and `truncated` repeat the result's. `truncationReason` is `response_bytes` iff the answer was reduced for size, otherwise `limit` iff `result.truncated`, otherwise null. The traversal is the contract's bounded ancestor-closed priority algorithm, with published-fact filtering, exact internal duplicate groups, `callSiteCount`, per-site `omitted` counts and sorted request-field `hints`. Root is always retained; unknown classifications stay visible. The requested root's caller role does not filter its own calls; exact internal test callees are still filtered unless `includeTests`. Non-root test callers remain subject to caller-role filtering, so their external calls need both flags. Published stale external bindings still count as external (owner rulings, 2026-10-28, #379). Filtering alone sets partial, not truncated; grouping alone sets neither. warnings-v1 is unchanged and uses retained evidence/representative bindings only. Only a fresh, exact, unambiguous internal `direct` or `constructor` binding with `staleTarget=false` from the selected producer expands; every other call is a boundary (`missingEvidence`, `stale`, `ambiguous`, `unresolved`, `external`, `dispatch`, `nodeLimit`). The result is static evidence, never a runtime trace.

**Producer scope (D8).** A selected producer applies only to documents in a language it declares. Under the per-language default (#483), each language's producer is its scope. A document outside that scope is answered from syntax evidence: its calls are `missingEvidence`, with no producer coverage row and no warning. A document in scope that lacks the producer's coverage row is corruption, which fails as `store_unavailable`, never a partial result.

**Failures** use the shared closed code set. `rootMissing` is `not_found` with the message `Call graph root not found in the admitted snapshot`; `producerUnavailable` (no row for the producer at the admitted revision) is `not_found` with the message `Semantic producer not published for the admitted revision`. A stale `expectedBasis` is `revision_conflict`, cancellation suppresses output and an expired deadline is `deadline_exceeded` (checked during node expansion and local-call inspection), and corrupt stored evidence is `store_unavailable`.

**Response bytes (D1, amended #379).** Build the bounded grouped answer once. Serialize and check the complete JSON-RPC frame first; return it unchanged if it fits. Only on overflow, make one forward pass over proper ancestor-closed priority prefixes. Add the next group only while the complete serialized JSON-RPC frame fits 65,536 bytes; stop at the first non-fitting prefix. No maximal-fit promise or descending exhaustive scan. Rebuild retained nodes/evidence/frontiers/warnings from frozen facts with no SQL or resolver re-evaluation. Both `data.result.request` and `data.requested` keep the original effective request and limits. Omitted immediate measured sites of retained owners receive `responseBytes` frontiers after depth/filtered/callLimit precedence, and `result.omitted.responseBytes` is their exact sum. Drop evidence/frontiers of pruned nodes. `truncated=true` and `truncationReason:"response_bytes"` disclose clipping. If the root plus mandatory closure/disclosure cannot fit, fail `range_too_large`. Existing 16 KiB request bounds remain unchanged. No graph config, new GraphError code, index/schema/publication change or warnings-v1 change is introduced.

**Imported-data reach.** The current SCIP importer sets `dispatch: unknown` on every call binding, so until dispatch evidence lands (#12, #14), `trellis_call_graph` on imported data stops at the root's direct callees.

**Project-named boundaries first (#778, owner-ratified 2026-11-07).** From a 2026-11-07 Claude Desktop session: without semantic evidence, compact `trellis_call_graph` from `src/http.rs::provisioned_core_answer` kept the first 60 of its calls in source order, almost all `Some`, `Ok`, `Err`, `clone`, `map_err` and `into_response`. `toolVersion` is **12** (previously 11). For this tool, the only reader of the engine, this amends step 5 of the contract-v1 [pinned bounded priority algorithm](semantic-evidence/contract-v1.md#pinned-bounded-priority-algorithm) by splitting its third rank, "other boundaries", in two:

- *Third:* a boundary call with a selected binding, or an unbound boundary call whose stored callee lookup key equals the lookup key of a declaration of the call's language in the admitted snapshot. This is the [Decision 0012](semantic-evidence/decisions/0012-incoming-calls-syntax-tier.md) name relation that incoming calls use, read from the call's side.
- *Fourth:* every other unbound boundary, including one with no provable callee key.

The ranking applies only to unbound boundary calls; semantic bindings always take precedence. A call with a selected binding keeps the placement that binding gives it. It expands in rank one, is an external leaf in rank two or, under the default `includeExternal:false`, is dropped as filtered. A dispatch, ambiguous, unresolved or stale binding stays in rank three whatever its name, so it never ranks below an unbound call. The name match is only the fallback order for calls with no selected binding: no producer for the call's language, a document whose evidence is pending (catching up or needing re-import), or no binding row for the call.

Ranks one (fresh admissible internal direct/constructor) and two (external leaves) are unchanged. The tie-breaks within a rank (owner depth, owner document and syntax identity, source order) are also unchanged. The name relation only orders candidates. It never targets, expands, filters or groups a call, and it changes no boundary reason, warning, `partial`, input or output schema. The `maxCalls` cut, its `callLimit` frontiers and `omitted.callLimit`, and the response-byte prefix stay exact, so a de-prioritized call that is cut is still counted. No standard-library name list is added. Sequence's external rule, a selected binding that resolves external, is already the call graph's `includeExternal` filter and is unchanged. A name match is not proof of a project target: a project `fn len` also lifts every `.len()` call.

Measured on a private clone of this repository at `913b645e`, syntax only, with compact defaults: the root has 339 calls, 60 are kept and `omitted.callLimit` is 279 both before and after. Among the 60 kept edges, `Some`, `Ok`, `Err`, `clone`, `map_err` and `into_response` went from 23 to 6. The remaining 6 are `into_response`, which this repository declares. The frame went from 25,445 to 25,597 bytes.

### `trellis_read_source` (W5)

Input `{schemaVersion:1,workspace?:string,path:string,startLine:UInt,endLine:UInt,expectedBasis?:Basis,expectedContentHash?:Hash}`; both guards may coexist and must pass. Indexed root-relative POSIX path, 1–4096 UTF-8 bytes: reject absolute paths, backslash, colon, NUL, and empty/`.`/`..` segments; no normalization or disk fallback. Data exactly `{path,contentHash:Hash,startLine:UInt,endLine:UInt,byteRange:Range,text:string,evidence:Evidence}`. Lines are 1-based inclusive, `1<=start<=end<=cached line count`. Locate lines by raw LF byte `0x0a` before decoding; preserve CRLF; final LF adds no empty line; empty file has zero selectable lines. Once guards and bounds pass, **strictly decode only selected cached bytes as UTF-8**. Invalid selection returns `unsupported_encoding`, retryable=false, safely verified currentBasis and null currentContentHash; never replacement U+FFFD, byte loss, transcoding, normalization, base64, changed cache/hash or live-file access. Invalid bytes elsewhere do not prevent a valid selected slice. Successful decoded text re-encodes byte-for-byte to that slice including terminators; contentHash is SHA-256 of the **whole original cached file**. At most 200 lines and 16384 raw selected bytes (equal to successful text UTF-8 length); oversized, reversed or out-of-file selections fail, never clip. Source bytes are not fabricated #22 provenance.

## Architecture references and reviewed rules (#129)

This additive surface leaves the six preceding tool inputs, outputs, evidence
meaning, clipping rules and errors unchanged. The three new flat closed inputs are:

| Tool | Exact input |
| --- | --- |
| `trellis_references` | `{schemaVersion:1,workspace?:string,expectedBasis?:Basis,paths?:string[],limit?:UInt,offset?:UInt}` |
| `trellis_rules_check` | `{schemaVersion:1,workspace?:string,expectedBasis?:Basis,paths?:string[],limit?:UInt,offset?:UInt}` |
| `trellis_rules_explain` | `{schemaVersion:1,workspace?:string,expectedBasis?:Basis,rule:string,target:Target,destination?:Destination,limit?:UInt,offset?:UInt}` |

`Target` is exactly `{sourceSetId,language,path,sid}` with native canonical SID,
full document identity and indexed checkout-relative path. `rule` is nonempty,
at most 256 UTF-8 bytes. Sources and rule `target` remain native. `Destination`
is the closed union of `Target` and
`{kind:"externalPackage",sourceSetId,language,package,standardLibrary}`. All five
external fields are required; `language` accepts `java`, `rust`, `python`,
`javascript` and `typescript`, `standardLibrary` is Boolean and package names are literal canonical
identities, not aliases, versions or globs. No native path, SID or metric is
invented for a package. `destination` is only valid for `$layers` and `$external`;
it selects the actual mapped destination identity, while unresolved-source layer
instances have no destination. Explicit null optional fields and unknown fields fail
`invalid_request`. `paths` are anchored positive source path globs, each 1–4096
UTF-8 bytes; omitted/empty selects all sources. Filters do not shrink the internal
full-snapshot destination map or target roles. References emit only the
native module/proof closure of selected sources (even those without sites),
emitted native production/test destinations and native intermediate proof modules,
in full document-key order (#347 owner amendment). The required `externalPackages`
array contains descriptors for emitted external destinations only. References use
`references:v2` and check/explain use `rules:v2`; these closed pre-release shapes
replace v1 without compatibility. Check/explain descriptors close all returned
external destinations and informational unassigned external identities, including
a preserved unassigned catalog after explain filtering.
A no-match source scope returns complete empty structural arrays and zero counts other than `unassignedFileCount`,
while `semanticComplete:false` remains invariant. Module revision/hash/provenance
links and all selected unresolved sites remain intact. Rules assign layers across
the full snapshot map. Their informational `unassigned` catalog follows `paths`
(owner ruling 2026-10-31, #559): with `paths` it lists the matching unassigned
modules and the unassigned external identities those sources observed; without
`paths`, everything. `unassignedFileCount` stays the checkout-wide number of
unassigned files either way. `limit` defaults to 5 and
accepts 1–50. For references, it is the number of **sample sites per edge**. For
check/explain, it is the number of **evidence entries per rule instance**: the
instance keeps exact `referenceCount`, status and constraints, and adds
`omittedEvidence` (the number of evidence entries not shown). It is never a cap on
edges, instances or counts. An out-of-range integer fails `range_too_large`. The
`trellis rules check` CLI keeps every evidence entry.

**Paging (#448).** `paths` selects sources before anything is measured, so unrelated
modules never consume response bytes. Each tool's arrays form one ordered sequence:

- references: `modules`, `externalPackages`, `production`, `tests`, `unresolved`;
- check/explain: `instances`, `testOnly`, `unassigned`, `externalPackages`.

A result holds the whole items of that sequence from `offset` (default 0, any safe
UInt) that fit in the 24,576-byte page target (#528; a lone larger item still fits the unchanged 65,536-byte response cap). The required
`page:{offset,nextOffset,total}` continues it: call again with the same arguments,
`expectedBasis` and `offset:nextOffset`, until `nextOffset` is null. `total` counts
every item. Every page carries the whole result's `counts`, `testCounts`,
`unassignedFileCount`, `limitations`, `rulesFile` and pin. Check also carries the whole
result's `apps` and `noApp` ([Amendment (#784)](#amendment-784-per-app-counts-in-rules-check)). The pages concatenate, in order, to the complete
selected result, and closures such as module and external-package descriptors hold
across the sequence, not within each page. An `offset` past `total` fails
`range_too_large`. Only an item that cannot fit even alone still fails atomically
with `response_too_large`. The cap is never raised, and no verdict, count or site is
clipped.

The exact closed payloads, ordering, module/site identity, counts, supported
structural mapping, grammar and exception semantics are in
[Reviewed architecture rules](rules.md#public-json-payloads). The payload pin
equals the outer `evidenceBasis`; there is no second head sample. The preceding
`coverage`/`provenance` array-closure requirements apply to find, declaration,
outgoing calls and incoming calls, not these new structural DTOs. The new DTOs
instead carry explicit native document-key/revision/hash/provenance links; these
identify the captured syntax and never prove the derived target mapping.
JavaScript and TypeScript output adds `npm`/`node` ecosystems within the existing closed v2
architecture payloads. npm descriptors use captured root package.json evidence;
Node descriptors use exactly `{kind:"standardLibraryCatalog",
catalog:"node-standard-library:v1",entry:"fs"|"path"}`. The
[Node source catalog](node-standard-library-catalog.md) pins the six accepted
specifier spellings and 56-root conservative npm exclusion. Other `node:` forms
stay unknown. Subpath labels do not prove members or export maps. Supported root
dependency sections/constraints, duplicate handling, workspace/self-reference
unknowns and intentional false negatives are specified in [rules.md](rules.md).
All retained manifest bytes come from the same selected admitted read as source;
no live filesystem, runtime inventory or installed package lookup is added.
JavaScript and TypeScript named/star export proofs use real final-owner native identities under
the shared four-edge limit; star collisions and external-name uncertainty retain
unknowns. These changes add no tools or protocol versions and do not alter search
or the seven non-architecture tool objects.

External descriptors instead carry identity, ecosystem and nonempty captured
manifest or reviewed standard-library catalog evidence, with the exact closed
shapes in rules.md. Java's `java-standard-library:v2` catalog pairs only with
entries `java`/`javax` and ecosystem `java`. The `java` root policy is unchanged;
`javax` requires an exact containing package in the
[pinned OpenJDK 25 public-export catalog](java-standard-library-catalog.md).
These remain package-family labels, not host-JDK discovery or proof that a class exists. Ordinary Java imports and
qualified-type paths can also map uniquely proved captured public top-level
types by declared package. Static/nested/member types and third-party prefixes
without authoritative captured mapping stay unknown; Maven/Gradle coordinates
alone are not prefix evidence. Python pairs `python-standard-library:v2` with
the exact 300 roots of the pinned CPython v3.12.0 `sys.stdlib_module_names`
source list and ecosystem `python`. The
[Python catalog record](python-standard-library-catalog.md) records its tag,
commit and source hashes. Upstream's cross-platform list includes unavailable
platform-specific and build-disabled modules, excludes test modules and lists
only main package names. These root labels are not host-runtime discovery,
installation, submodule or member proof. Unlisted roots and unproved third-party
distribution/import mappings stay unknown. Local precedence and ambiguous or unproved roots cannot fall through
to external labels. Python local expansion requires unique checkout-root modules
and proved package prefixes; literal `__all__` selects proved names, otherwise
proved public top-level names are used only when `__all__` is absent. Dynamic or
mutated exports, unsupported string decoding, ambiguous layouts and cycles stay
unknown. Simple proved forwarding follows at most four edges to real native
final owners; independent positives retain residual unknowns. External stars do
not provide a Python member inventory or settle wildcard-only names. No runtime
execution or third-party import-name guessing is performed. Tuple `__all__` is
unknown under the literal-list-only contract. Child-module imports such as
`from . import x` or `from pkg import sub` can still map only to the initializer;
missing child edges are a follow-up. Dynamic writes via module `setattr`,
`vars()[...]` or `__dict__.update` are not fully tracked, so no complete dynamic
namespace-mutation detection is claimed.
Captured manifests are selected through the same
admitted transaction and revision as source, never read live. Package evidence is not
native measured syntax provenance or library member/API resolution. Browser source
and metric joins use genuine native modules only. These DTOs add no fabricated
snapshot or semantic relationship proof. Code comes only
from captured bytes/native rows in that admitted snapshot. Structural mapping is
syntax, not compiler resolution; `semanticComplete:false` is invariant. Where a
language has a selected producer (#483), fresh call and type bindings also map
sites, each marked `mapping:"semantic"` with its binding's provenance, and each edge
reports its `evidenceTier` and `semanticSites` (#523; see
[rules](rules.md#semantic-bindings-523)). Stored
Decision 0011 roles separate production and tests: feature-only `test-support`
remains production. Call/type constraints use only fresh selected bindings and stay
unknown without them, never invented positive bindings. Peer/shared-outbound policy
must be explicit reviewed rules, not inferred defaults.

The three tools retain the resolved envelope/selection/admission failure forms,
`structuredContent` and deterministic at-most-512-byte summary. References counts
both emitted production and test edges, using `edges`/`edge`; check/explain counts
production `instances`/`instance`. New successful results always have
`partial:true,truncated:false,truncationReason:null`. They warn `syntaxOnly` with null
provenance unless the answer rests on semantic evidence: the payload's `evidenceTier` is
`mixed` (references with semantic sites, #523), or a check/explain instance or constraint
cites a `semanticBinding` or `typeRelationship`, counted before evidence sampling (#664). Bounded site samples retain exact counts/omissions in the payload;
this is not top-level verdict truncation or proof of semantic completeness.

Only these new output schemas add nonretryable codes `invalid_rules`,
`rules_not_found`, `bounded_work` and `response_too_large`. They also add the retryable
`building`, with its own failure branch ([#798](#amendment-798-a-typed-building-answer-from-the-architecture-tools)). A file with a syntax error is a
`partialFiles` row, not a failure (owner ruling 2026-10-31, #563).
A located `invalid_rules` failure has the ordinary resolved error fields **plus**
required `diagnostic:{code,kind,path,range,line,column,attribution,message}` as
[defined for rules](rules.md#work-bounds-and-located-failures). Other failures do
not acquire this field. No partial instances accompany malformed configuration,
work failure or overflow. The full JSON-RPC line, summary and LF must
fit 65,536 bytes. Larger results page as described above. An item that cannot fit
alone fails atomically with `response_too_large` rather than clip verdicts,
unresolved sites, proof or TOML. This does not change the original
tools' existing prefix/`range_too_large` behavior. Only static `tools/list` retains
the 262,144-byte catalog cap/headroom requirement.

### One no-argument proposal prompt

Both modern discovery and legacy initialize advertise `prompts:{}` beside
`tools:{}`. No sampling or elicitation capability is added. `prompts/list` is static,
workspace-independent and has no cursor/pagination. Modern params contain only
required protocol `_meta`; the modern result is
`{resultType:"complete",ttlMs:0,cacheScope:"private",prompts:[Prompt]}`.
Legacy list accepts absent/empty params and returns `{prompts:[Prompt]}`.
There is one entry, with `name:"trellis_propose_rules"`, a description and
`arguments:[]`.

`prompts/get` accepts exactly `{name:"trellis_propose_rules",arguments?:{}}`
plus modern required protocol `_meta`. It accepts no workspace/pin/path arguments
and always uses the launch checkout, not a previously selected worktree. Unknown
name, argument, cursor, extra field, wrong type or missing modern metadata fails
JSON-RPC `-32602`. Legacy initialization/initialized requirements still apply.
The prompt name is not a tool and is not accepted by `tools/call`.

Get follows the same admitted code snapshot, protocol-acceptance deadline, call
capacity, cancellation and final-root/selection fences as evidence calls. Its
result is `{description,messages:[{role:"user",content:{type:"text",text}}]}`;
modern adds `resultType:"complete"`. It is **not** a CallToolResult: no `isError`,
`structuredContent` or tool-summary wrapper. Operational failures are JSON-RPC
`-32603` with bounded `error.data.code` and, for `invalid_rules`, the located
diagnostic; cancellation suppresses unsent outcomes. The ordinary 65,536-byte
complete-frame cap applies, not the catalog cap.

Prompt content separates server-authored task instructions from untrusted
repository data. It supplies the verified checkout, one code pin, sampled
`catchingUp`, structural references/limitations and production/test split (as the
bounded summary of [Amendment (#770)](#amendment-770-a-bounded-prompt-reference-view)); it also
supplies current exact rules bytes and the separate checkout-config descriptor,
or explicit absence. Missing rules permits a new proposal; malformed rules fails
atomically, just as check/explain do. The index pin does not identify current
configuration bytes. (#783: the prompt also lists detected apps and asks for
layers per app; see [Amendment (#783)](#amendment-783-per-app-layers-in-the-proposal-prompt).
#794: the prompt is `trellis_propose_rules`, lists authored apps too and explains how to
author them; see [Amendment (#794)](#amendment-794-authored-apps-and-the-trellis_propose_rules-prompt).)
Output asks for explicit integer orders, reviewed peer/shared
choices, justified placements, exact positively witnessed baseline exceptions and
a `rules.toml` diff **for human review**. Ambiguities go to normal chat. Trellis
never writes the proposal, invokes a provider, runs repository code or requests
indexing. This wire contract makes no production Claude Code interoperability or
interactive-display certification.

### `trellis_show` (#818)

**Input**, closed: `{schemaVersion:1,workspace?:string,view:"sequence"|"class"|"source",symbolId?:SyntaxId,lookupKey?:string,path?:Path,line?:UInt,expectedBasis?:Basis}`. `sequence` and `class` take exactly one of `symbolId` or `lookupKey` and no `path` or `line`; `source` takes `path` (as `trellis_read_source` admits it) and `line` (from 1) and no `symbolId` or `lookupKey`. Any other combination, value or field is `invalid_request`, before any read. The `view` field is this tool's own; the inspect tools still have no `view` discriminator.

**Checks**, in the call's admitted snapshot, in the order the #787 deep link applies them:

1. **Stale pin.** An `expectedBasis` other than the admitted head is `stale`. Without one, the admitted head is used.
2. **Existence.** A `symbolId` with no indexed declaration, a `lookupKey` that matches none, or a `path` with no indexed file is `not_found`.
3. **Bounds.** A lookup key that matches several declarations is `invalid`. For `sequence`, the declaration must be a function or method, and the browser's own sequence read (`seed`, at the admitted pin) must admit it with that exact seed. For `class`, the browser's class-diagram read must admit it as a class, or a method with a supported enclosing class; its refusal message is the answer's. For `source`, `line` must be a line of the cached file: every typed reason the line cannot be read (past the end, over the 16 KiB read cap, not UTF-8) is `invalid`, never a failure.

A target that passes is recorded only at the call's final successful send decision, after every check the call can still fail: its deadline, its cancellation and the final identity check of its root (#824). It is then `shown` with its `generation` if a viewer is open, otherwise `no_viewer` and nothing is recorded. A call that answers `deadline_exceeded` or `root_changed`, or is cancelled, records nothing; nothing recorded is ever undone, so a later show is never overwritten.

**Success envelope**, closed: `{schemaVersion:1,requestId,workspace,catchingUp,evidenceBasis:Basis,data:{outcome,view,generation:UInt|null,message:Text},warnings:[],partial:false,truncated:false,truncationReason:null}`. `outcome` is `shown`, `no_viewer`, `not_found`, `stale` or `invalid`; `generation` is non-null exactly for `shown`; `message` is static server text; `evidenceBasis` is the admitted snapshot, so a `stale` answer names the basis to read again at. All five outcomes are successful answers; the text summary is `trellis_show: <outcome> <view>; full result in structuredContent.`

**Failures** use the shared closed code set and envelopes: malformed arguments are `invalid_request`, and index, storage, capacity, selection and deadline failures are what they are for every tool. A failure records nothing.

## Result completeness, bounds and failure precedence

Find/declaration/outgoing `partial` iff truncated, returned selected coverage incomplete, or a returned boundary call. Find and search also set `partial:true` and one aggregate `coverageIncomplete/null` for selected partial/failed native coverage anywhere in the searched scope, even on zero hits. Vector incompleteness changes only `searchIndex`, never native `partial` or warnings. Search always includes `syntaxOnly/null`. Thus nonempty syntax-only outgoing calls are partial without a dedicated warning. Source success is never partial/truncated; describe follows its status rule. For evidence responses apply [#22 warnings-v1](semantic-evidence/contract-v1.md#warning-completeness-warnings-v1) with selection `semanticProducerId=null`: `syntaxOnly/null` is required even for empty find and source. Selected failed/partial coverage adds `coverageIncomplete/null`. Other triggers are #22 stale provenance per ID, possiblyStale aggregate/null, staleTarget per binding provenance and ambiguous binding per provenance; unreachable under valid fresh syntax-only first release. Deduplicate `(code,provenanceId)`; order by declared enum **coverageIncomplete, staleEvidence, staleTarget, bindingAmbiguous, syntaxOnly**, then null-before-text provenance and message. No unresolved/unsupported/truncation warning.

Request cap 16384 bytes counts UTF-8 JSON-RPC line excluding delimiter. Response cap 65536 counts the **complete serialized JSON-RPC line including full `structuredContent`, the summary text block and the LF delimiter**. The cap applies to every response except `tools/list`. The catalog is static and authored by Trellis, not derived from repository content, so its complete line, including LF, has its own **catalog cap of 262144 bytes** (#165); a test keeps the catalog at least 16384 bytes under it. `Limits.responseBytes` describes the 65536-byte cap that bounds tool results. For the original tools with list clipping, choose the longest fitting ordered item prefix and recompute its emitted-only document table/refs, required coverage/provenance closure, warnings and omission counts for each prefix; never trim identity, mandatory coverage or provenance. Find's entire searched-scope `scopeCoverage` is fixed overhead, even for zero hits. For outgoing, first choose the longest ordered **call** prefix that fits with each `nativeCandidates.items:[]`, each emitted call’s exact known `omitted` count or null budget result, the emitted-call document table and mandatory closure. Calls excluded from that prefix contribute nothing to `omitted`. Then fill candidate prefixes for calls with known counts in call order (each at most eight) while reserializing the **whole JSON-RPC line including structured content, summary text and LF** and adding shown declarations' document refs, required coverage/provenance closure; a null-count call keeps its items empty. Stop the global candidate prefix at the first item that would exceed 65536 bytes; count that and all remaining verified items, plus each call's unexamined eligible matches, as omitted. Never drop an already selected call or required closure just to show a hint. If fixed coverage/envelope, or the first required call and its empty candidate object/closure, cannot fit, return `range_too_large`; never shrink a searched scope or report a falsely complete empty result. A shown item never lacks its native closure. Single-object/source outputs fail rather than truncate. The three #129 architecture tools and prompt fail atomically as specified above, never as a prefix of verdicts or TOML. A **call** omitted for response bytes sets `truncated:true` and reason `response_bytes`; calls beyond the request `limit` set `truncated:true` and reason `limit`. **Top-level truncation describes the required calls only (#167):** a hint omitted for the eight-item cap or for response bytes is reported only in that call's `nativeCandidates.omitted` and never sets top-level `truncated` or `truncationReason`. Any **known** omitted candidate sets `partial:true`; a null omitted count alone does not assert truncation, a warning or additional partiality (a returned unresolved boundary call already has `partial:true`). `complete:false` stays invariant even with zero known omissions. Truncation reason priority is `response_bytes`, then `limit`, then `callee_text`. Exactly meeting a cap is not truncated; clipped callee sets partial/truncated even when all call rows fit. Candidate omission adds **no warning**.

**Admission and limits (Decision 0009 §7):**
- **Deadline:** a `tools/call`'s five-second deadline starts at **protocol acceptance**, when the relay (or the daemon session, for a direct socket client) reads the complete valid call. It covers application validation, capacity, selection, read admission, the read and the send decision.
- **Capacity:** after application validation, a connection may have at most four outstanding calls, and a fifth receives `too_many_requests` without queueing; its message names the per-connection limit (#528). The daemon also runs at most 16 live call-worker threads; a worker keeps its permit until its thread exits, even after its call timed out or was cancelled. Exhausting that budget, or failing to start a thread, also gives `too_many_requests`.
- **The send decision:** the daemon rechecks cancellation, then the deadline, then selected-root identity. The relay decides each call's client-facing outcome by its own acceptance plus five seconds. The bound covers that decision, not a client that stops reading stdout.
- **After the deadline:** if mandatory call/read work exceeds it, return `deadline_exceeded` with no partial evidence unless cancellation suppresses output. Optional hint work and the optional search vector arm stop at their separate budgets instead of failing the mandatory answer; search reports `unavailableReason:"vector_budget_exceeded"`.

Before read admission has sampled `catchingUp`, a `too_many_requests` or `deadline_exceeded` uses the pre-resolution failure shape:

`{schemaVersion:1,requestId,error:{code,message,retryable:true,attemptedWorkspace,currentBasis:null,currentContentHash:null}}`

`attemptedWorkspace` is the selected string, or the launch workspace path when the selection is omitted, in the bounded `path` encoding. After read admission, `deadline_exceeded` keeps the resolved failure shape.

| Typed error code | Condition / retryability |
| --- | --- |
| `invalid_request` | Bad application field, path, ID, partial/malformed pin, reversed range; false |
| `range_too_large` | Numeric cap, source/response size, out-of-file range; false |
| `unsupported_encoding` | Selected cached source slice invalid UTF-8; false |
| `revision_conflict` | Complete basis or found cached path hash mismatch; false (change guards) |
| `index_not_ready` | No valid published head; true |
| `not_found` | Valid ID or cached path missing; false |
| `too_many_requests`, `deadline_exceeded` | Admission cap or deadline; true |
| `rebuild_in_progress` | A large publication has spilled to `index.db` and holds every reader of the prior head out until it commits (#210); the read waits up to 10 s for that commit and then reads the new head (a pinned read keeps its pin), and fails with this only if the publication still holds the index (#674); true |
| `root_changed` | Opened workspace root pathname missing or its `(device,inode)` differs, including the final response check; false for this opened workspace, `currentBasis:null` |
| `store_unavailable` | Unsafe/unreadable/invalid store or missing required coverage; false (no automatic same-call retry) |
| `workspace_selection_failed` | No answering checkout; typed reason `not_absolute`, `not_checkout`, `identity_changed` (false), or `unavailable` (true). Attempted workspace is bounded and tagged. |
| `checkout_capacity` | Verified checkout could not attach within the 64-active-checkout bound; true. |
| `daemon_unavailable` | Daemon connection/start/reconnect unavailable after bounded attempts; true. |
| `outcome_unknown` | Acknowledgement for an ambiguous mutation was lost; false, inspect request/job status instead of replaying automatically. |

For each original tool, the closed output-schema failure enum includes **all fifteen** codes above, including `unsupported_encoding`, `root_changed`, `workspace_selection_failed`, `checkout_capacity`, `daemon_unavailable` and `outcome_unknown`. Its closed top-level success and failure shapes have `workspace` and `catchingUp` only after resolution. Before resolution, ordinary argument failures (`invalid_request` or `range_too_large`) have neither answering fields nor an attempted workspace; invalid-workspace-type failures additionally include a tagged `attemptedWorkspace`. Selection and pre-resolution admission failures also have a tagged `attemptedWorkspace` and no answering fields. Each of these closed shapes is a branch of every tool's output schema, so `structuredContent` alone validates on either outcome. Sanitize error messages: no SQL, source, credentials or absolute roots. After protocol acceptance, validate application fields, then apply capacity (Decision 0009 §7), then check the opened root pathname and `(device,inode)` before read admission, readiness, basis comparison, object lookup, hash comparison and range/materialization, in that order; repeat the root check at final eligibility before sending. A moved/replaced/missing root at either check takes `root_changed` over a pin, lookup, or unsent success and returns `currentBasis:null`. Startup gives `index_not_ready` before comparing a complete pin; stale complete pin conflicts before not_found; hash guard follows successful cached path lookup. A selected encoding error is not store corruption and requests no repair. Cancellation overrides all unsent outcomes.

## Snapshot eligibility (W8)

Discover/list operate during startup. Describe reports status; evidence tools need an eligible snapshot. The daemon alone opens and writes the index and queue; its checkout worker owns startup reconciliation, publication and FIFO claims. For every daemon-routed evidence read, admit a committed, validated head for the selected checkout (including a valid prior head during startup reconciliation, watcher work or queued FIFO work). Check the opened root pathname against its captured `(device,inode)` before and after materializing one coherent SQLite snapshot, and check the selected root again at the daemon send decision. Discard rejected responses. A missing/replaced opened root returns `root_changed`, retryable=false with `currentBasis:null`; an unsafe, unreadable or invalid store returns `store_unavailable` with no rejected `currentBasis`; no valid published head returns `index_not_ready` with `currentBasis:null`. There is no leader/incarnation/follower check or predecessor permit. `catchingUp` is advisory, sampled at read admission; it neither invalidates a validated prior head nor authorizes an unvalidated one. A normal later publication does not invalidate a coherent in-flight snapshot whose root and selected-read checks pass; guards compare **inside that snapshot**. No mixed graph/source revision, historical fallback or linearizable filesystem freshness.

| Situation | Eligibility |
| --- | --- |
| Daemon unavailable | Connect/start fails with typed `daemon_unavailable`; no standalone index read. |
| Cold activation or reconciliation with no valid published head | Describe reports status; evidence returns `index_not_ready` with `currentBasis:null`. |
| Startup reconciliation, watcher work or pending FIFO with a valid prior head | Evidence may read that committed, validated head with `catchingUp:true`, subject to the same root and selected-read checks; this grants no queue-claim authority. |
| Startup reconciliation commits a new validated head | Subsequent reads can use the new coherent basis; a complete pin to the old basis conflicts. |
| Opened root pathname removed/moved/replaced, before or after materialization or at send decision | Discard evidence; `root_changed`, retryable=false and `currentBasis:null`. Do not follow the new occupant. |
| Unsafe/unreadable/invalid store at unchanged root | Discard evidence; `store_unavailable` with `currentBasis:null`. |
| Rebuild/new generation | Old complete generation pin conflicts at an eligible new basis; incompatible old store is never a fallback. |
| Normal publication while reading eligible revision R | In-flight response can remain at coherent R after publication R+1 if root and selected-read checks pass; pin compares at R. |

## Hand-reviewed contract checks (not executed tests)

The compact projections below use `B={indexGeneration:"123e4567-e89b-42d3-a456-426614174000",indexRevision:7}`, `S="sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"`, `H="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"`. These are **illustrative valid-shaped tokens**, not computed #22 identity vectors. `E={evidenceTier:"syntax",semanticBasis:null,freshness:"fresh",staleBecause:[]}`. `success(data,B,partial,truncated,reason,warnings)` abbreviates the **exact resolved success** envelope above, with schemaVersion=1, matching outer requestId, `workspace:"/verified/launch/root"`, `catchingUp:false` and expanded data schema; it never authorizes omitted fields in a real response. `failure(code,basis,hash)` abbreviates the exact **resolved failure** envelope with the same answering workspace and catching-up fields, sanitized nonempty message, retryable per table and matching ID. Replace those two fields with the actually selected verified root and sampled catching-up state for explicit selection or a pending H. Neither shorthand applies to selection or invalid-workspace-type failures, which have no answering fields. Example `Coverage`/`Provenance` rows must expand every #22-required nested field; shorthand names below refer to complete valid rows, not alternative schemas.

**W1 status.** `tools/call` arguments `{schemaVersion:1}` on startup yields success data `{workspaceLabel:"checkout",indexState:"indexing",progress:{completed:0,total:null},watcherDegraded:false,languages:[],toolVersion:7,schemaVersion:1,limits:{requestBytes:16384,responseBytes:65536,sourceBytes:16384,sourceLines:200,defaultLimit:20,callDefaultLimit:6,maxLimit:50,pageBytes:24576,calleeTextBytes:1024,hintWorkMs:1000,deadlineMs:5000,concurrency:4},searchIndex:{backend:null,unavailableReason:null}}` for a standalone registry without configuration knowledge, basis=null, partial=true, warnings=[], truncated=false, reason=null. Ready returns five language rows in declared order, progress=null, non-null B; selected partial/failed counts make partial=true. `{schemaVersion:1,expectedBasis:B}` fails invalid_request.

**W2 find and pins.** `{schemaVersion:1,query:"Foo"}` defaults to 20. Exact folded name ranks ahead of prefix, then substring or ID-only; original name/null then ID breaks ties. Each returned Symbol carries a `doc` referencing its one table entry and inclusive `lines` for its measured `range` (null only for a zero-length empty-file item), plus a qualified `displayKey`, e.g. `src/a/Invoice.java#Invoice.total` from measured path/ancestors/name, not merely `total`; unnamed keys use the defined kind/position fallback. The label is non-unique (Java overloads may share it) and does not affect rank. For a symbol whose range is `{start:0,end:5}` in selected cached `"ab\ncd\n"`, `lines:{start:1,end:2}` and a referenced table entry with `document.path:"src/a.rs"` and the whole-file hash `5141648ccbe924f6462cfc7085ccd21779b89d8cee1438281bf1b4cd8d63ac2a` allow W5 with the same B and hash to return `byteRange:{start:0,end:6}`; the symbol range is contained. A zero-length empty-file module instead has `lines:null` and no selectable W5 lines. At limit=50, a reply holds at most 50 symbols and never exceeds `responseBytes`. **Exactly 50 matches** returns all 50, truncated=false, only when they fit; otherwise the longest fitting prefix, truncated=true, reason=response_bytes. **51 matches** returns the first 50, truncated=true, reason=limit, partial=true when they fit; otherwise the longest fitting prefix with reason=response_bytes, which outranks limit. Either way, `documents` contains exactly the returned symbols’ referenced document versions (zero-based refs in sorted full-key/revision order); coverage and provenance are the closure of only those symbols, and `scopeCoverage` counts the full searched scope. `limit:51` fails range_too_large; `expectedBasis:{indexRevision:7}` fails invalid_request; an old complete generation/revision conflicts before lookup. For `query:"absent"`, native JavaScript A selected complete and B selected failed with required diagnostic return `symbols:[],documents:[],coverage:[],provenance:[]` with `scopeCoverage`'s javascript row `{selected:2,complete:1,partial:0,failed:1,unselected:0}` and no row for any other language (#715), partial=true, truncated=false, reason=null, warning keys `[(coverageIncomplete,null),(syntaxOnly,null)]`. B selected partial has the same outcome; both selected complete give partial=false and `[(syntaxOnly,null)]`. An additional unselected unsupported tuple U counts as `unselected:1` and **alone** changes neither partial nor warnings. A zero-hit answer over a scope of hundreds of documents fits, because its size no longer depends on the scope; missing B still fails store_unavailable.

**W3 declaration.** `trellis_declaration` with `{schemaVersion:1,symbolId:S,expectedBasis:B}` returns the projected complete #22 declaration with `syntaxId:S`, a non-null qualified `displayKey` from its path/ancestors/name (or measured unnamed fallback), data-level `doc:0`, `lines:{start:1,end:2}` for an illustrative declaration range `{start:0,end:5}` in cached `"ab\ncd\n"`, E, `documents:[{ref:0,document:{sourceSetId:"app",language:"rust",path:"src/a.rs"},revisionId:"r1",contentHash:"5141648ccbe924f6462cfc7085ccd21779b89d8cee1438281bf1b4cd8d63ac2a",provenanceId:"native-proof"}]` and relevant full coverage and provenance closure. The nested declaration retains all other #22 fields but omits its document/revision/provenance fields; the table contains the whole-file hash. A W5 read of lines 1–2 with B and that table hash selects byteRange `{start:0,end:6}`, containing `{start:0,end:5}`. No source body. A body edit may retain S but change hash/basis; B then conflicts before ID lookup. Unknown canonical S gives not_found. `limit:1` or `view:"declaration"` fails invalid_request.

**W4 outgoing.** `trellis_outgoing_calls` with `{schemaVersion:1,symbolId:S}` defaults 6 (#528); measured calls ordered path/range/ID, each ownerSyntaxId=S. A nested callable's call belongs to its **own** syntax ID and is excluded. The selected symbol’s `displayKey` is its non-null qualified label, such as `src/a/Invoice.java#Invoice.total`. One unresolved measured call has binding=null, disposition=unresolved, targetId=null, boundaryReason=missingEvidence, E, partial=true, `displayKey` equal to the owner label plus `@byte:<call.range.start>` and optional bounded calleeText (e.g. `src/a/Invoice.java#Invoice.total@byte:412:foo`), no dedicated unresolved warning (syntaxOnly still required). No calls **with complete relevant coverage** returns `calls:[]`, partial=false; no calls with failed/partial selected coverage has coverageIncomplete and partial=true, not a complete-negative claim. Cached callee text over 1024 bytes clips on scalar boundary, calleeTextTruncated=true, truncated/partial=true, reason=callee_text unless response_bytes/limit takes priority. No calleeRange means calleeText=null, calleeTextTruncated=false. A verified single identifier `Call.spelling:"foo"` with no eligible native declaration returns `nativeCandidates:{items:[],omitted:0,complete:false}`; so does unverified compound spelling `obj.foo` (no heuristic stripping), a legacy-only ID or missing optional hint proof. A measured member-name `foo` with `calleeRange` and full spelling both verifying that token may match, but its containing `obj.foo()` expression alone cannot. With one verified same-snapshot name match, `items` contains that declaration’s actual `syntaxId,name,doc,range`, where its table entry supplies matching document, revision, hash and **that declaration’s native proof**; the native coverage/provenance closure remains full, `omitted:0,complete:false`; binding, targetId and disposition stay null/null/unresolved. With nine verified matches in sorted order, show eight, `omitted:1,complete:false`, partial=true, and leave top-level truncation to the calls. If the complete response cannot fit a candidate, keep only the fitting global prefix and count all other eligible verified matches for emitted calls in omitted, again without top-level truncation; excluded calls never contribute to omitted. Candidate clipping emits no extra warning. If required selected call/declaration coverage, provenance, digest or revision is corrupt, fail store_unavailable rather than return an empty hint. If optional matching/count cannot finish within the 1000 ms hint-work budget, discard partial hints for that emitted call, return `items:[],omitted:null,complete:false` for it and unprocessed emitted calls, preserve the outgoing call answer, and add no warning or truncation solely for this unknown hint count; a mandatory call/read timeout may still return deadline_exceeded. An empty list never proves no callee, and even an eight-item list never licenses a graph edge. For a call range `{start:3,end:5}` in cached `"ab\ncd\n"`, `lines:{start:2,end:2}` and the referenced table hash from W3 permit a W5 read with B and that hash; its byteRange `{start:3,end:6}` contains the call range. A call with no emitted candidates still has its own referenced table entry; an empty `calls:[]` has `documents:[]` despite retained owner coverage/provenance. `view:"outgoing_calls"` fails invalid_request; `limit:51` fails range_too_large.

**W5 source.** `{schemaVersion:1,path:"src/a.rs",startLine:1,endLine:1,expectedBasis:B,expectedContentHash:H}` returns exact selected bytes and E with basis B, syntaxOnly warning, partial=false, truncated=false. For raw cached `61 0a ff 0a`, the whole-file SHA-256 is `ead0215fe443756c2739e50b368ff79c283d540214d4ef503350fa25103198be`. Lines 2–2 select `ff 0a` and fail `unsupported_encoding`, retryable=false, currentBasis=B, currentContentHash=null, **no data or replacement text**. Lines 1–1 succeed with text `"a\n"`, byteRange `{start:0,end:2}` and the same whole-file hash despite invalid bytes elsewhere. Neither opens disk nor changes cache. Wrong hash conflicts before decoding and reports found cached hash; over-cap selection fails range_too_large before decoding. A CRLF line returns existing `\r\n` bytes unchanged. Reject `../a`, `/a`, `a\\b`, `a:b`, zero/reversed lines and >200 lines as typed above; final LF creates no phantom line.

**W7 result summaries.** For a success with 13 emitted calls and optional hints clipped for response bytes, the text is `trellis_outgoing_calls: 13 calls; full result in structuredContent.`; hint clipping is not top-level truncation (#167). When the call list itself is cut for response bytes, the text is `trellis_outgoing_calls: 13 calls, truncated: response_bytes; full result in structuredContent.` In both cases `structuredContent` retains every emitted call, its evidence, the exact omitted hint counts and the complete success envelope. A count of one uses the singular item word: `trellis_find_symbols: 1 symbol; …`, `1 call`, `1 line`. For an invalid declaration input, the text is `trellis_declaration failed: invalid_request: Invalid tool arguments`; `structuredContent` retains the closed failure envelope. The other success item words are `workspace`, `symbols`, `declaration` and `lines`; for each tool a failure uses its actual code and message. Never decode summary text as JSON or treat it as the result.

**W6 protocol fixture transcripts.** For otherwise valid requests under the 16384-byte line cap, outer string ID `"a"` and `"\u0061"` have the same decoded ID and 3-byte canonical token. A string ID of 254 ASCII `a` characters or 127 `"` characters has a 256-byte canonical token including quotes and is admitted; 255 `a` characters (257 bytes) or 128 `"` characters (258 bytes) receive bounded JSON-RPC `-32600` with `id:null` before mode selection or pending registration. A string ID of 63 `😀` characters (254 UTF-8 bytes with quotes) is admitted; 64 (258 bytes) receives the same error. An ID of 252 ASCII `a` characters plus a decoded newline has a 256-byte canonical token and is admitted whether the newline arrived as `\n` or `\u000a`; 253 `a` characters plus newline is 257 bytes and receives the same bounded error with `id:null`. Malformed UTF-8/JSON and whole-line oversize retain their existing error handling.

Modern sequence: request `server/discover` with `params:{_meta:{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}` → discovery with `resultType:"complete"`, `ttlMs:0`, `cacheScope:"private"` and `_meta["io.modelcontextprotocol/serverInfo"]`; same `_meta` on `tools/list` → `resultType:"complete"`, `ttlMs:0`, `cacheScope:"private"` plus twelve ordered entries (`trellis_workspace_describe`, `trellis_find_symbols`, `trellis_search_symbols`, `trellis_declaration`, `trellis_outgoing_calls`, `trellis_incoming_calls`, `trellis_read_source`, `trellis_call_graph`, `trellis_references`, `trellis_rules_check`, `trellis_rules_explain`, `trellis_show`); `tools/call` `{name:"trellis_workspace_describe",arguments:{schemaVersion:1},_meta:<same literal-key object>}` → `resultType:"complete"` plus full structured content and summary text. Legacy sequence: `initialize` with revision `2025-11-25`, capabilities `{}`, clientInfo → negotiated response; `notifications/initialized` → list → call using connection state, with legacy list/call results omitting `resultType`. Missing modern metadata `-32602`; unsupported modern revision `-32022` with `error.data:{supported:["2026-07-28","2025-11-25"],requested:<unsupported-revision>}`; unknown tool `-32602`; premature legacy call `-32600`. An opened root replaced during a pending call produces tool `root_changed` with retryable=false and currentBasis=null, even if an earlier basis or pin matched. Cancellation suppresses a pending result; a mandatory admitted-call timeout returns deadline_exceeded unless cancelled, while optional hint-budget exhaustion returns omitted:null without error; fifth concurrent call returns too_many_requests; oversized complete response uses longest fitting list prefix or range_too_large for fixed coverage/first item. W3 `{schemaVersion:1,symbolId:S,limit:1}` and `{schemaVersion:1,symbolId:S,view:"declaration"}` produce application `invalid_request`; W4 accepts `{schemaVersion:1,symbolId:S,limit:50}` and rejects `view` as `invalid_request`. The old `trellis_inspect` name receives protocol `-32602` as an unknown tool. These are review fixtures **for #17**, not executed client or runtime interoperability.

**W8 selection and prior-head checks.** For a client launched in `/repo/main`, `{schemaVersion:1,workspace:"/repo/linked"}` on each of the twelve known tools selects the verified linked worktree, and `workspace:"/other/repo"` selects that verified unrelated checkout the same way (#839): no Git common directory has to match. Each resolved success or failure has top-level `workspace:"/repo/linked"` (or `"/other/repo"`) and `catchingUp:<sampled boolean>`; omission selects `/repo/main`, not the last explicitly selected checkout. While startup reconciliation is pending, a committed, validated prior head may return its basis B with `catchingUp:true` after the ordinary root and selected-read checks; no valid published head returns resolved `index_not_ready` with `currentBasis:null` and `catchingUp:true`; an unsafe or invalid store instead returns `store_unavailable` without a basis. Once reconciliation commits a new validated basis, an old complete pin conflicts against that basis. Neither a prior-head read nor `catchingUp` authorizes FIFO claims before reconciliation commits. A symlink alias `/repo/alias` to a checkout has `{schemaVersion:1,requestId,error:{code:"workspace_selection_failed",message:<sanitized nonempty message>,retryable:false,reason:"not_checkout",attemptedWorkspace:{kind:"path",value:"/repo/alias",utf8Bytes:11,truncated:false},currentBasis:null,currentContentHash:null}}` with **no** top-level `workspace` or `catchingUp`. A non-string `workspace:[]` yields `{schemaVersion:1,requestId,error:{code:"invalid_request",message:<sanitized nonempty message>,retryable:false,attemptedWorkspace:{kind:"invalid_type",jsonType:"array"},currentBasis:null,currentContentHash:null}}`, not a JSON-RPC error. An unavailable attempted selection has reason `unavailable`, retryable=true and the same bounded path tag; neither an error nor a truncated 4097-byte input silently selects a different checkout. These are contract examples, not executed runtime tests.

**W9 warning checks.** Empty complete syntax evidence → `[(syntaxOnly,null)]`, partial=false. Selected partial/failed → `[(coverageIncomplete,null),(syntaxOnly,null)]`, partial=true. Unresolved outgoing boundary → partial=true without special warning. Callee clipping → truncated/partial with reason `callee_text` and without coverageIncomplete. A **known** verified native-candidate omission → partial only: it is reported in that call's `nativeCandidates.omitted` and never sets top-level truncated (#167), and adds no hint-specific warning. Unknown optional hint count (`omitted:null`, items empty) alone causes no warning or truncation; a returned unresolved call remains partial because it is a boundary. As a consistency check only, #22's semantic examples of stale provenance and ambiguous binding would add staleEvidence/bindingAmbiguous at their declared keys and order; **not emitted by first-release syntax evidence**. A failed extraction does not invent provenance.

**W10 historical original-five closed tool check.** In the original five-tool release, `trellis_declaration` and `trellis_outgoing_calls` have flat, closed input objects with no `view` field: each `{view:"incoming_calls"}`, `{view:"call_paths"}`, `{view:"usages"}`, `{view:"type_hierarchy"}`, `{view:"implementations"}`, `{view:"coverage"}` (with otherwise valid required fields) fails application `invalid_request`, as do even the former `declaration` and `outgoing_calls` values. `trellis_inspect` is unknown, not a compatibility alias. That original five-tool release has no incoming reservation or placeholder. The sixth standalone `trellis_incoming_calls` is now specified above; it does not accept `view` and makes no guessed caller, exact target or reverse-completeness claim. #60’s five other relations remain gated by their existing prerequisites and will be separate tools, not `inspect` views. [#11](https://github.com/SquashMerge/trellis/issues/11) separately ratifies evidence-backed later relations/views. Earlier `SPEC.md` and `docs/agent-integration-plan.md` two-view prose reflects the superseded four-tool catalog and is aligned separately during this cutover; do not rewrite the superseded historical pilot implementation plan. An added declaration does not automatically resolve historical semantic calls; #23 promises no fixed incremental publication deadline.

## Amendment (#620): native-first and labelled older-basis semantic reads

[Owner-approved 2026-11-03](https://github.com/SquashMerge/trellis/issues/620#issuecomment-6610642350). The [projection addendum](semantic-evidence/native-first-delete-design.md#addendum--approved-older-basis-projection-2026-11-03) governs this exception to the no-mixed-revision read rule: a coherent native-N snapshot may carry **explicitly identified** older-basis P semantic evidence, never unlabelled mixed authority or N-valid old bindings. This applies equally to the browser, CLI and MCP closed output schemas. Request bounds and source read pin/hash guards do not change.

Per-document semantic read status distinguishes current evidence, `olderBasis`, `catchingUp` (retained input that semantic catch-up will finish) and `needsReimport` (no retained input; owner ruling #620, 2026-11-03). Graph `boundaryReason` adds `needsReimport` beside `catchingUp`. An older-basis entry carries its semantic paired pin and native Revision alongside the response’s native paired pin. It retains original producer/capture/proof/target identity and historical basis; effective freshness is at most `possiblyStale`, preserving `stale`. This includes compact items and independently detachable nested graph/sequence/rule evidence. A top-level `catchingUp` flag alone is insufficient labelling. A pending selection is not a fabricated successful producer coverage row; required tuples missing without its validated marker still fail typed.

Unchanged documents must retain eligible prior-pin evidence. Changed documents and actual internal-target/candidate/dispatch/type/context dependents are pending; opaque producer lookup keys are not assumed to map to native identifier names. Symbol/declaration lookup, references, outgoing/incoming calls, sequence and semantic rules return eligible P evidence with its basis labels. Each semantic hop verifies eligible endpoints against N and stops at a changed/dependent endpoint with a partial boundary or separately labelled native-N fallback. Do not rewrite a P internal target to N, perform name-based re-resolution, silently upgrade an external claim, carry #657’s exact-native freshness grant across N, or claim current semantic absence from historical/pending results.

Native source/structure still uses N and its committed cached bytes. Retained N replies preserve their recorded pending/projection decisions after S. The projection uses a retained validated basis directly, not an unbounded overlay chain. The sole checkout worker publishes semantic S only against its exact durable target, advancing indexRevision in the same generation/native Revision. No second publisher or client-side refresh is introduced.

### `trellis_endpoints` (#851)

**Input**, closed: `{schemaVersion:1,workspace?:string,expectedBasis?:Basis,app?:string,method?:"GET"|"POST"|"PUT"|"DELETE"|"PATCH"|"HEAD"|"OPTIONS",pathTemplate?:string,limit?:UInt,offset?:UInt}`. `app` and `pathTemplate` are 1–4096 bytes; `limit` is 1–50 (default 20) and counts rows per page; `offset` continues a page. An unknown field, a wrong type, a lowercase or other method is `invalid_request` naming its field; `limit` out of range and an `offset` past `total` are `range_too_large`.

**Rows.** The [#788](semantic-evidence/contract-v1.md#interface-evidence-amendment-788) extraction of the admitted revision's architecture core, read whole and never re-extracted: every server route and every client call, in two runs. First each route in extraction order, `matched` when at least one call reached it and `serverOnly` when none did; then each call that reached no route, `clientOnly`, in call order. A row is `{list:"matched"|"serverOnly"|"clientOnly",method:Text|null,pathTemplate:Text|null,app:Text|null,handler:Handler|null,callSites:Site[],callSiteCount:UInt}`:

- `method` and `pathTemplate` are the route's, or the call's for `clientOnly` (null when unreadable; a client substitution is `{}`). `app` is the route's app, or the call's for `clientOnly`, by name; null is no app.
- `Handler={kind:"declaration"|"closure"|"dispatchArm",syntaxId:Text|null,stableLookupKey:Text|null,path:Text,lines:{start,end},variant:Text|null}`, as #788 defines it; null for `clientOnly`.
- `Site={path:Text,lines:{start,end},confidence:"exact"|"template"|"wrapperInferred"|"unresolved",caller:{name:Text,stableLookupKey:Text}|null,app:Text|null}`: a `matched` row's call sites, in call order, with their #788 confidence; a `serverOnly` row has none; a `clientOnly` row has its one call, `unresolved`. `lines` are 1-based and inclusive.
- `callSites` holds at most 5 sites per row, or at most 50 in the single-endpoint form; `callSiteCount` counts them all.

**Filters**, all optional and combined: `app` keeps a route row when its handler or any of its call sites is in the app, and a `clientOnly` row when its call is; a row keeps all of its call sites. `method` and `pathTemplate` keep the rows whose method and template equal them exactly (`/api/items/{id}`, not a client path). With `pathTemplate` the answer is the **single-endpoint form**: one endpoint's handler and callers, a `matched` or `serverOnly` row per route of that method and template, and any `clientOnly` row of the same text. No row is not an error.

**Data**, closed: `{evidence:"structural",extracted:boolean,limitations:Text[],endpoints:Endpoint[],indexGeneration,indexRevision,page:{offset,nextOffset,total}}`. `limitations` always says that rows are structural matches by method and path template, not proof of what runs, and that a route with no call site or a call with no route proves nothing. When the checkout has fewer than two detected plus authored apps, nothing is extracted: `extracted` is false, `endpoints` is empty and `limitations` adds "not extracted: fewer than two apps". The envelope is the architecture tools': `partial:true`, the `syntaxOnly` warning, `evidenceBasis` at the read's pin.

**Paging and bounds.** Rows page like `trellis_rules_check` instances (#725): at most `limit` rows and about 24 KiB per page, at least one row, `nextOffset` null on the last page. A lone row larger than the page still answers under the 65,536-byte response cap; otherwise the call fails `response_too_large`. The work is the core's, bounded as #788 bounds it (`bounded_work`).

**Rules and failures.** A rules file is optional; with one, its rejection is `invalid_rules` with no `diagnostic` (check locates it). While the owner computes the revision the answer is the #798 `building` failure. The failure schema is `trellis_references`'.

### Closed projection DTOs (#620)

`SemanticProjection` is exactly `{currentPin:Basis,semanticBasisPin:Basis,basisNativeRevisionId:Text,semanticStatus:"olderBasis"|"catchingUp"|"needsReimport"}`. `currentPin` is the response’s native N snapshot; the semantic pin/native Revision identify the retained validated destination P, not the original capture basis. `DocumentSemanticStatus` is `{producerId:Text,language:Language,path:Text,projection:SemanticProjection}`. These read-status fields do not add a producer coverage state or modify captured proof bytes.

An independently returned projected proof wrapper is `{evidence:T,projection:SemanticProjection,historicalFreshness:"fresh"|"possiblyStale"|"stale",effectiveFreshness:"possiblyStale"|"stale"}`. `T` retains its P proof/target identity. `stale` stays stale; historical fresh/possiblyStale becomes effectively possiblyStale. Read-facing graph/call/sequence/rule items carry their projection even when compact mode omits proof arrays. Native presentation provenance may also carry an optional `projection` for a semantic display label; native measured facts do not thereby become P semantic evidence. Raw `semantic_evidence::Provenance` input/storage shape is unchanged.

<a id="amendment-660-semanticstatus-on-evidence-answers"></a>
## Amendment (#660): semanticStatus on evidence answers

**Status:** implemented (owner ruling 2026-11-08 on #660, D11). **On the wire at `toolVersion` 26** (previously 25, #855). It does not depend on automatic producer re-runs. The `tools/list` frame limit is the 235,520-byte compacted-frame limit (owner ruling 2026-11-09, #851); the measured modern and legacy frame sizes at `toolVersion` 26 are recorded in the catalog note below.

- `trellis_call_graph`, `trellis_outgoing_calls`, `trellis_incoming_calls` and `trellis_declaration` gain a required top-level `semanticStatus:[{language:Language,state:"catchingUp"|"needsReimport",reason:Text}]`. It lists each language of the answer's evidence whose semantic state at the selected snapshot is `catchingUp` or `needsReimport`, in #22 language order, and is `[]` when there is none. It is distinct from `SemanticProjection.semanticStatus` (#620), which labels one projected item.
- It is projected from the same per-language state as describe `languages` (`LanguageStatus.semantic`, including the [#632](#amendment-632-retained-semantic-re-import-status) `reimport` reason), in the same selected snapshot. `reason` carries that existing typed reason; `catchingUp` covers retained input that S will finish and, with #660 automatic re-runs, a scheduled or running re-run ([contract v1](semantic-evidence/contract-v1.md#automatic-reruns-660)).
- **Exact shape.** Each item is `{language,state,reason}` with `language` one of `java`, `rust`, `python`, `javascript`, `typescript`, `state` one of `catchingUp`, `needsReimport` and `reason` a string; no other key. The catalog's success branch of each of the four tools adds `semanticStatus` to `required` and defines it as an array of that closed object (a local `$ref` to the branch's `Language` where it has one). The array is `[]` when no language is pending.
- **Language set.** The languages are those the tool passes to the syntax-only tier. A tool that does not call it takes the root or declaration document plus every document the answer names (its `documents` table, or the call graph's node documents).
- **Reason vocabulary.** The #632 re-import reasons, `re-import pending`, `re-import required`, `producer re-run scheduled` and `automatic re-run failed: <code>`.
- **Failure envelopes** never carry it, and it takes part in each answer's frame-fit computation like any other top-level field.
- It is informative only: it changes no evidence, coverage, freshness, completeness, `partial` or boundary meaning, and MCP still never runs a producer or initiates an import. MCP smoke treats it as informative.
- **Catalog.** Only the four output schemas and the `toolVersion` constant change (25 to 26). The compacted-frame limit is 235,520 bytes; the measured sizes at `toolVersion` 26 are: modern `tools/list` frame 233,572 bytes and legacy 233,515 bytes, against the 235,520-byte limit.
