# Semantic symbol search: contract-change proposal

Status: **proposed, not ratified.** This document lists the contract changes that semantic symbol
search ([#211](https://github.com/SquashMerge/trellis/issues/211)) would need. It amends nothing.
The ratified documents keep governing until the owner rules on each item below, and each ruling
lands as an amendment to the record it changes, or as a new decision record:

- [`semantic-evidence/contract-v1.md`](semantic-evidence/contract-v1.md) (#22);
- [`local-topology.md`](local-topology.md) (#23);
- [`mcp-readonly-pilot-contract.md`](mcp-readonly-pilot-contract.md) (#24);
- the decision records in [`semantic-evidence/decisions/`](semantic-evidence/decisions/).

Where this proposal and a ratified record differ, the ratified record wins.

The design direction is [`semantic-search-plan.md`](semantic-search-plan.md), which is also
unratified. This proposal follows the narrower scope agreed on #211:

- one vector per declaration, built from a symbol card;
- no AST code chunks at first;
- hybrid ranking with FTS5 BM25 over split identifiers.

The evaluation set is in [`semantic-search-eval/`](semantic-search-eval/). Benchmark results are
summarized on #211.

## Summary of owner decisions

Each item below ends with its open questions. The decisions they add up to are:

| # | Decision | Proposed answer |
| --- | --- | --- |
| D1 | Tool name, and whether adding a tool bumps `toolVersion` | `trellis_search_symbols`; bump `toolVersion` to 2 |
| D2 | Query embedding on the MCP read path | Allowed, for a local pinned model only; never a hosted provider |
| D3 | Search-index status | A separate `searchIndex` object; #22 coverage, freshness, `partial` and warnings are unchanged |
| D4 | Revision pinning | Same as W2: one eligible snapshot, the same `expectedBasis` rules, no new conflict class |
| D5 | Vector store | Per-user content-addressed cache `<cache>/vectors.db`; `index.db` holds only the FTS5 table and card hashes |
| D6 | Card facts that extraction doesn't capture today | Captured during native extraction into search-only tables, behind a native producer version change |
| D7 | Model provenance | `modelId` pins name, revision, weight and tokenizer hashes, dimensions, quantization, prompts and card format |
| D8 | Model distribution | A user-configured model path, verified against the pinned hash; no bundled weights and no download |
| D9 | Retrieval, not evidence | Hits have no relevance provenance or score; agents verify with `trellis_declaration` |
| D10 | Size budget | int8 vectors at ≤1024 dimensions; a size cap on `vectors.db`; FTS5 measured against #199 |
| D11 | Remote embedding (voyage-code-4) | Out of scope until the owner opts in; it would need its own contract change |
| D12 | Shared FTS5 with #199 item 5 (Q-8) | One identifier FTS5 table if it can keep W2's literal-substring semantics; otherwise two tables |

## 1. New MCP tool and catalog version

**Touches.**

- `mcp-readonly-pilot-contract.md`, status line and opening paragraph: "All six tools use cached
  index state … They never request indexing, start a producer/build, run repository code, open a
  terminal, invoke a provider or make a durable write." and "The catalog order is
  `trellis_workspace_describe`, `trellis_find_symbols`, `trellis_declaration`,
  `trellis_outgoing_calls`, `trellis_incoming_calls`, `trellis_read_source`."
- Same document, § Protocol, negotiation and transport: `tools/list` returns "six entries in
  catalog order"; the summary item words are "`workspace`, `symbols`, `declaration`, `calls`, and
  `lines`".
- Same document, § Shared schema and limits: "Workspace selection applies to the six tool calls
  only"; "All six flat input schemas retain `additionalProperties:false`".
- Same document, § W1: describe data contains `toolVersion:1` and `limits:Limits`.
- Same document, § Result completeness: "The closed output-schema failure enum includes **all
  fourteen** codes above".
- `local-topology.md` § T07: "The five cached read-only MCP tools, in order, are …". This sentence
  already predates Decision 0012's sixth tool. The amendment that adds search should correct both.
- `src/mcp/catalog.rs` `NAMES: [&str; 6]` and the reviewed fixture
  `tests/fixtures/mcp/catalog.json`.

**Proposed change.** Add a seventh standalone, read-only, closed tool, `trellis_search_symbols`,
after `trellis_find_symbols` in catalog order. It follows Decision 0012's precedent: a separate
tool, not a `view` of another tool, and not an `inspect` union.

- **Input:** `{schemaVersion:1, workspace?:string, query:string, limit?:UInt, expectedBasis?:Basis}`.
  - `query` is 1–1024 UTF-8 bytes of natural language or identifiers, with no trimming or
    normalization.
  - `limit` defaults to 10 and accepts 1–50.
  - There are no filters at first. W2 has none either, and the plan's language, path, kind and
    test filters can be a later, separately reviewed amendment.
- **Data:** `{hits:SearchHit[], documents:DocumentRef[], searchIndex:SearchIndexStatus,
  scopeCoverage:LanguageStatus[]}`.
  - `SearchHit` reuses W2's `Symbol` (with `doc`, `range`, `lines`, `role` and the syntax
    `Evidence` of the declaration itself) and adds `rank:UInt` and
    `matchedBy:("lexical"|"vector")[]`.
  - `documents` and `scopeCoverage` follow W2's rules: the emitted-only document table, and
    four-row scope counts.
- **Summary text:** `trellis_search_symbols: <n> hits; full result in structuredContent.`, with the
  item word `hits`.
- **Bounds:** W2's 65,536-byte response cap, its longest-fitting-prefix rule, and the five-second
  deadline.
- **Failures:** the existing fourteen codes. No new failure code is needed, because a missing
  model or an unfinished vector job is a state, not a failure (§3).
- **Catalog version:** bump describe's `toolVersion` from 1 to 2. `schemaVersion` stays 1, because
  no existing schema changes.

**Why.** Search must be agent-callable to replace the exploratory grep loop, and the catalog and
its fixture are closed. The catalog adds about 13 KB to a 73 KB compact catalog. That fits the
262,144-byte catalog cap (#165) with ample margin.

**Open questions.**

1. Is `trellis_search_symbols` the right name? The plan used `search_symbols`. Every tool has the
   `trellis_` prefix.
2. Should adding a tool bump `toolVersion`? Decision 0012 added the sixth tool without a bump, so
   the precedent is "no". A bump lets a client detect the catalog change from describe alone.
3. Should search reuse `trellis_find_symbols` with a mode flag instead? This proposal says no:
   W2's ranking and literal-substring contract are exact, and mixing in similarity ranking would
   weaken them.
4. **Query embedding on the read path.** "Never … invoke a provider" holds for a local in-process
   model. But embedding the query is new compute inside the five-second deadline, and a cold
   model load can exceed it. Should the daemon keep the model resident? Or may search answer
   lexical-only, with `searchIndex.state:"unavailable"`, while the model loads?

## 2. Card content and native extraction

**Touches.**

- `semantic-search-plan.md` § What gets embedded (unratified).
- `contract-v1.md` § Records and bindings: the closed `Declaration` record.
- `local-topology.md` § T05: "Publish graph, source bytes, input inventory, dependency
  observations and generation/revision atomically."
- Decision 0012 § Compatibility, as the precedent: "Change the native producer version and rebuild
  the index; do not migrate it."

**Proposed change.** A card holds:

- the name, kind, qualified display key, root-relative path and signature (from the stored
  header, parameters and header items);
- the doc comment;
- body facts: callee spellings (from `native_version_calls`), types named in the signature and
  body, string literals, error-code-shaped tokens in literals, and member names (for containers);
- for card variant (c) only, a capped head of the declaration's source.

Native extraction stores headers, parameters and call spellings today. It does **not** store doc
comments, string literals or body type names. The proposal is:

- capture those three during native extraction, from the same tree-sitter tree and the captured
  bytes, with no second parse and no file reread;
- store them in **search-only** tables keyed by `(document_version_id, syntax_id)`. They are not
  fields of #22's `Declaration`, `Header` or any evidence record;
- bound them: at most 20 literals of at most 100 bytes each, a 600-byte doc comment, 30 type
  names and 40 callees per declaration;
- bump the native producer version, which rebuilds the disposable index under T06. The R-2 ruling
  on #199 already requires that version to be computed at build time from everything that affects
  extraction output.

**Why.** On the benchmark, body facts were what let a card match a concept that lives only in an
undocumented or poorly named body (see #211). The facts must come from extraction so cards are
reproducible from a publication, and so search never rereads files.

**Open questions.**

5. Should the search-only facts live in `index.db` beside the declaration rows, or should they
   be computed only when a card is built, and never stored? Storing them costs index size (#199);
   not storing them means a rebuild of the vector cache needs the tree again.
6. Should the card format be versioned separately from the native producer version
   (`cardFormat` in `modelId`, §6)? This proposal says yes, so a card-format change re-embeds
   without re-extracting.

## 3. Search-index status, separate from #22 coverage and freshness

**Touches.**

- `contract-v1.md` § Identity and coverage: "`complete` means all requested, supported evidence
  for that document."
- `contract-v1.md` § Basis and freshness: "Freshness is independent from coverage." The states are
  `fresh | possiblyStale | stale`, with "no numeric confidence".
- `contract-v1.md` § warnings-v1: the closed warning codes.
- `mcp-readonly-pilot-contract.md` § Result completeness: "Find/declaration/outgoing `partial` iff
  truncated, returned selected coverage incomplete, or a returned boundary call."
- `mcp-readonly-pilot-contract.md` § Shared schema: the closed `Warning.code` enum.

**Proposed change.** Add a closed wire type, used only by the search tool:

```text
SearchIndexStatus = {
  state: "complete" | "building" | "partial" | "unavailable",
  modelId: Text | null,
  declarations: UInt,   // eligible declarations at the answering snapshot
  embedded: UInt,       // of those, with a current vector for modelId
  lexical: "complete" | "unavailable"
}
```

- `complete` means `embedded == declarations`. `building` means a background embedding job is
  running and `embedded < declarations`. `partial` means no job is running but some vectors are
  missing (evicted, failed or over a bound). `unavailable` means there is no configured or
  verified model, and ranking is lexical-only.
- A hit without a vector is still returned when it matches lexically, with `matchedBy:["lexical"]`.
- The status is computed from the answering snapshot. It never claims vector coverage the
  snapshot doesn't have.
- **#22 is unchanged.** Coverage tuples, freshness, `Evidence`, warnings-v1 and the envelope
  `partial`/`truncated` meanings stay exactly as defined. A search hit's `Evidence` describes the
  declaration's own syntax record (as in W2), not the hit's relevance. Search-index incompleteness
  appears **only** in `searchIndex`. It does not set `partial`, and it adds no warning code.

This replaces the plan's `searchIndex.indexPin` and `embeddedPin` fields. Vectors are content
addressed (§5), so "embedded as of revision N" isn't meaningful. What matters is how many
declarations *at the answering snapshot* have a vector, which the counts state directly. The
envelope's `evidenceBasis` already names the snapshot.

**Why.** #22's coverage says whether *evidence* was extracted. Whether a background vector job has
finished is a different fact. Reusing coverage, `partial` or a warning for it would make every
search during a rebuild look like an evidence failure, and would weaken those signals for the
other tools.

**Open questions.**

7. Should vector incompleteness also set the envelope's `partial:true`? This proposal says no,
   because `partial` is about evidence. An agent that wants complete vector coverage reads
   `searchIndex.state`.
8. With no model configured, should the tool answer lexical-only (proposed), or fail? Answering
   keeps search useful on machines without a model.

## 4. Revision pinning

**Touches.**

- `semantic-search-plan.md` § Contract fit: "The local topology already classifies 'symbol
  search, ranked/global queries' as operations that conflict when stale." **This is no longer
  true.** That classification ("Outgoing calls, symbol search, ranked or global queries and
  negative results always …", and the table row "Find symbols, and any ranked or global query |
  Never") was added in `85ac5dc` and removed in `83d19d7` (the second local-topology review
  round, 2026-09-23). The current rules are:
  - `local-topology.md` § T02: "A revision pin is the complete pair
    `{indexGeneration,indexRevision}` … every pinned read … requires and atomically checks both."
  - Decision 0006 § 1: retained revisions (head plus 15 minutes) stay readable at their pin. "A
    pinned read or write at a released revision fails with a typed conflict … It never silently
    moves to a newer revision."
  - `mcp-readonly-pilot-contract.md` § W8: "Each admitted evidence result uses one checkout and
    one coherent, committed and validated snapshot"; "stale complete pin conflicts before
    not_found".

**Proposed change.** Search follows W2 exactly, with no new conflict class.

- **One snapshot.** One eligible snapshot answers the whole query: lexical candidates, the card
  hashes of eligible declarations, and the `documents` and `scopeCoverage` rows.
- **Pins.** With `expectedBasis`, the snapshot is that pin, or the call returns
  `revision_conflict` under the existing rules. Without it, the answer uses the eligible head and
  reports it in `evidenceBasis`.
- **Vectors.** Vectors are looked up by the card hash stored *in that snapshot*. A vector embedded
  for an identical card at another revision is the same vector, so no cross-revision evidence is
  involved. A missing vector only lowers `embedded` (§3).
- **Ranking.** The ranking is global within that snapshot. It is never stitched across revisions,
  and a later publication never moves an in-flight answer.

**Why.** Search returns IDs that the agent then passes to `trellis_declaration` with the same
`expectedBasis`. The pair is consistent only if search used that snapshot. No new rule is needed,
just the existing W2 and W8 rules applied to a new tool, and the plan's citation should be fixed.

**Open question.**

9. Should search *require* `expectedBasis`? This proposal says no: it is optional, as in W2,
   because the first search usually starts a session.

## 5. Where vectors live, and their invalidation

**Touches.**

- `local-topology.md` § T02, the fixed-locations table. Its rows are the derived index, use lock,
  explicit requests, optional facts (`<cache>/facts.db`), durable record and use lock, and tokens
  and ledgers.
- `local-topology.md` § T02: "The fact cache cannot be authoritative: a missing, corrupt,
  conflicting or unavailable entry falls back to extraction".
- `local-topology.md` § T06: "Optional facts may be evicted without changing evidence."
- Decision 0007 (one per-user daemon) and Decision 0008 (the daemon is the sole index and queue
  writer).
- #199, which reports `index.db` at about 125× the source size.

**Proposed change.**

- **New T02 row.** "Optional vectors | `<cache>/vectors.db` | Path-neutral card vectors keyed by
  `(modelId, cardHash)`. Misses and eviction change ranking only, never evidence, and are reported
  in `searchIndex`."
- **`index.db`** holds, per document version (so #67's version sharing applies):
  - the FTS5 identifier table;
  - each declaration's `cardHash`.

  `cardHash` is SHA-256 over a new domain, `trellis.search-card.v1\0`, and the exact card bytes.
  Vectors are never stored in `index.db`.
- **The embedding job.**
  - **Ownership.** The daemon is the only writer of `vectors.db`, as of every other topology
    store. It runs one embedding owner per daemon, shared by all checkouts, with one resident
    model.
  - **When it runs.** It runs after publication and never delays it. It is not part of the T04
    FIFO stream, and has no durable queue state: after a restart, the pending set is recomputed
    as the cache misses for the current head.
- **Invalidation is by key only.**
  - A changed card (a body, doc, signature, path or literal change) has a new `cardHash`, so it
    misses the cache.
  - A changed model, prompt, dimension, quantization or card format has a new `modelId`, so
    everything misses.
  - Nothing is deleted on publication.
- **Eviction.** Size-capped LRU, as #71 does for facts. An index rebuild or recreation keeps the
  cache, and a new worktree of the same commit reuses every vector (cards contain only
  root-relative paths).
- **Corruption (T00).** Decoding is strict: the blob length must equal `dimensions × bytes per
  component`, and the `modelId` must match. A failure is a miss, and the entry is re-embedded.
  SQLite integrity failures follow T00's typed corrupt failure for the cache, and search answers
  lexical-only. No per-vector checksum: under T00 and CLAUDE.md, Trellis trusts what it wrote and
  validated. This narrows the plan's proposed vector-blob checksum.

**Why.** Vectors are pure functions of `(model, card text)`, so sharing them per user, like
facts, avoids re-embedding for each worktree or rebuild. Keeping them out of `index.db` avoids
adding to the size problem #199 is working on.

**Open questions.**

10. Per-user cache (proposed), or `index.db`? `index.db` would make vectors part of the atomic
    publication, but it would multiply storage by worktrees, and a rebuild would drop them.
11. What should the default cache cap be, and should it be configurable? See §8 for sizes.
12. Should `trellis gc --report` list `vectors.db` size and entry count? It lists derived indexes
    today, and facts are not listed.

## 6. Model provenance pinning

**Touches.**

- `contract-v1.md` § Identity and coverage: `Producer.id`, `.version`, `.executableHash`.
- `semantic-search-plan.md` § Model: "The model is pinned like a producer".
- `local-topology.md` § T02: "Tokens and Jev/ACP ledgers | Explicitly configured external paths".

**Proposed change.**

- **`modelId`.** It is SHA-256 over a domain `trellis.search-model.v1\0` and the canonical JSON of
  `{name, revision, weightsSha256, tokenizerSha256, dimensions, quantization, maxTokens,
  queryPrompt, documentPrompt, pooling, cardFormat}`.
  - `revision` is the upstream commit, for example the Hugging Face commit hash.
  - `weightsSha256` covers every weight file, in sorted-name order.

  The wire reports the hex `modelId` and the `name`.
- **Unknown models are refused.** Trellis ships a closed table of supported `(name, revision,
  weightsSha256)` triples. A configured model whose hashes don't match is reported as
  `searchIndex.state:"unavailable"`. It is never loaded under another identity.
- **Weights.** They come from an explicitly configured path, like tokens and ledgers. Trellis
  never downloads them (D8). The weights are hashed once when the daemon loads the model, then
  pinned for that daemon's lifetime.
- **Not a producer.** The model is not a #22 producer and gets no `Producer` row, because its
  output is not evidence (§7). Its pin exists for cache correctness and reproducibility.

**Why.** Vectors are only comparable within one exact model, prompt and card format. A silent
weight or prompt change would mix incompatible vectors in one ranking.

**Open questions.**

13. Should Trellis bundle a default model, or require the user to configure one? Bundling makes
    search work out of the box but adds hundreds of megabytes to the distribution. Configuring
    keeps the binary small.
14. Which inference runtime: ONNX Runtime or candle? Both run in-process. ONNX Runtime adds a
    native library, and candle adds build time. The benchmark used PyTorch and ONNX Runtime only
    to measure quality. A Rust runtime's speed still needs its own measurement.
15. Some candidate models (voyage-4-nano, jina-embeddings-v2-base-code) publish custom Python
    modelling code. A Rust runtime would reimplement the architecture rather than run that code.
    Is a model whose reference implementation is custom code acceptable?

## 7. Results are retrieval, never evidence

**Touches.**

- `contract-v1.md` § Records and bindings and § Stable identifiers: "Display keys are presentation
  only … never accepted as input"; durable anchors: "Never name-search replacement";
  `DurableAnchor.targetId`: "Searching/replacement invalid".
- `mcp-readonly-pilot-contract.md` § W2: "do not fabricate [provenance] for the name-match
  relation, failed extraction or negative search"; "Empty symbols cannot prove complete
  extraction."
- Decision 0012 § 2: "This name relation has no borrowed producer proof."

**Proposed change.** State in #24, and in the tool's description string, that:

- a hit is a **candidate** to inspect. Its relevance has no provenance, no #22 record and no
  freshness label. The declaration's own syntax `Evidence` and `doc` describe only that the
  declaration exists at the snapshot, as in W2;
- the wire carries **no numeric similarity score**, only `rank` and `matchedBy`. Scores are
  model-specific and invite reading as confidence, which #22 rules out ("no numeric confidence");
- zero hits, or a missing expected hit, proves nothing about absence;
- search results never feed #22 records, durable anchors, anchor reattachment, Jev or ACP
  packets, or any other tool's evidence. Agents verify a hit with `trellis_declaration` and
  `trellis_outgoing_calls` at the same `expectedBasis`;
- the plan's optional Jev rerank, if ever added, follows the same rule. Its labels are retrieval
  metadata.

**Why.** The core #22 guarantee is that evidence is measured or produced with provenance, never
inferred. Similarity is inference.

**Open question.**

16. Should hits expose scores for debugging behind a non-default flag? This proposal says no.

## 8. Size budget

**Touches.**

- #199 "index size" (item 7): `index.db` is 16.8 GB for 134 MB of source at 10,000 files.
- `semantic-search-plan.md` § Storage and ranking: "100k chunks × 768 dimensions is about 77 MB".

**Proposed change.** Budget, per #211's first scope:

- **Vectors (`vectors.db`, per user).** One int8 vector per named declaration. At 1024
  dimensions, that is about 1.05 KB per declaration including the key. This repository has
  6,722 named declarations in 315 indexed documents: about 7 MB at 1024
  dimensions, or 3.7 MB at 512. Scaled by declarations per file to the #199 10,000-file
  corpus, that is about 225 MB at 1024 dimensions. This proposes:
  - a default cap of 1 GiB, with LRU eviction;
  - at most 1024 dimensions, int8.
- **FTS5 (`index.db`, per checkout).** One row per named declaration per document version. In the
  benchmark, a standalone FTS5 table over the variant (b) card text (3.9 MB of text, porter
  tokenizer, optimized) is 5.8 MB for this repository: about 1.6% of this repository's 362 MB
  `index.db`, for 8.3 MB of source. A production table indexing fewer fields (names, path,
  callees and literals) should be smaller. It must be measured against #199's numbers when built,
  and must not ship before #199's size items.
- **Embedding time** is a background cost, not an indexing cost. See #211 for measured
  full-repository times per model.

**Why.** Vectors could otherwise grow the index #199 is shrinking. A per-user cache with a fixed
cap bounds them independently of the number of checkouts.

**Open question.**

17. Should a full-dimension mode (2048 for voyage-4-nano) be allowed? The benchmark shows what
    truncation costs.

## 9. Threat model (T00) fit

**Touches.** `local-topology.md` § T00.

**Proposed change.** No new defenses beyond T00. The in-scope items are:

- **Untrusted repository content.** Card building is bounded (§2), and model input is truncated
  to the model's `maxTokens`. Card text is data: it is embedded, never executed or interpreted.
- **Untrusted callers.** The search input is closed and bounded (§1), and the query is truncated
  to `maxTokens` before embedding.
- **Ordinary faults.** The embedding job may crash or be killed at any point. The cache is
  re-derivable, so a partial write or eviction is a miss.
- **Accidental corruption.** Strict decoding (§5); a cache failure answers lexical-only and never
  returns a partial hit list presented as complete.

Out of scope, per T00: signing or authenticating cached vectors against a same-user process.

## 10. Remote embedding arm

**Touches.**

- `mcp-readonly-pilot-contract.md` opening: tools never "invoke a provider".
- `semantic-search-plan.md` § Model: "A remote embedding API is available only as an explicit
  opt-in."
- The live Jev rules (`live-jev.md`): explicit opt-in, a key from the environment, and a budget
  ledger.

**Proposed change.** None now. voyage-code-4 sends repository code, and query text, to a hosted
service. It is **out of scope** until the owner opts in. It would then need:

- its own amendment, because embedding a query at read time would be a provider call;
- a key from the environment;
- a ledger;
- a separate `modelId`.

The #211 benchmark did not run it.

**Open question.**

18. Does the owner want the remote arm measured at all, and on which repository?

## 11. Coordination with #199 item 5 (Q-8, symbol search speed)

**Touches.**

- `mcp-readonly-pilot-contract.md` § W2: "literal name/ID substring … Use SQLite built-in ASCII
  `lower` and literal `instr`."
- #199 item 5: "Symbol search: a narrow table or trigram index (Q-8)."

**Proposed change.** None to W2. FTS5 with the `trigram` tokenizer can accelerate W2's
literal-substring matches of three or more bytes, but W2 must still return byte-identical results
(shorter queries fall back to a scan). The hybrid ranker wants a different tokenizer: identifiers
split at camelCase and snake_case, with stemming. One table can't serve both semantics exactly.
The likely result is two FTS5 tables: a trigram table for W2 and a split-identifier table for
search. Both are per document version.

**Open question.**

19. Is one shared table worth W2's exactness risk? This proposal says no: two tables.
