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
| D3 | Result closure and search-index status | W2's emitted-only `coverage`/`provenance` closure and searched-scope `partial`/`coverageIncomplete` rule, unchanged; vector completeness only in a separate `searchIndex` object |
| D4 | Revision pinning | W8 as it stands: the validated head only; an old complete pin conflicts, and a later `trellis_declaration` at the returned basis may conflict after a new head. Reading a retained older revision would be a separate #24 change, not proposed |
| D5 | Vector store and its corruption | Per-user cache `<cache>/vectors.db`; `index.db` holds only the FTS5 table and card hashes. A database integrity failure fails the call with `store_unavailable`. A row that fails decoding is an owner decision: typed failure per T00, or a narrow T00/T02 amendment making it a miss (§5) |
| D6 | Card facts that extraction doesn't capture today | Captured during native extraction into search-only tables, behind a native producer version change. Cards and ranking use measured fields, never `displayKey` |
| D7 | Model provenance | `modelId` pins name, revision, weight and tokenizer hashes, dimensions, quantization, prompts and card format |
| D8 | Model distribution | A user-configured model path, verified against the pinned hash; no bundled weights and no download |
| D9 | Retrieval, not evidence | Hits have no relevance provenance or score; agents verify with `trellis_declaration` |
| D10 | Size budget | int8 vectors at ≤1024 dimensions; a size cap on `vectors.db`; FTS5 measured against #199. Whether search waits for #199's size items is an owner decision |
| D11 | Remote embedding (voyage-code-4) | Out of scope until the owner opts in; it would need its own contract change |
| D12 | FTS5 and #199 item 5 (Q-8) | Two tables: a trigram prefilter for W2 (every hit still verified by W2's literal rule) and a split-identifier table for search. **The W2 prefilter is ratified by [Decision 0013](semantic-evidence/decisions/0013-symbol-search-trigram-prefilter.md);** the search table stays proposed |
| D13 | Hybrid ordering | A fixed, deterministic fusion and tie-break rule (§1), part of the contract; changing it is a contract change. BM25 is scored over a head-only FTS5 table. The query transformation is total. No cross-call output promise by default; whether to add a cross-call determinism guarantee later is open question 6, to be specified and reviewed if the owner asks for it. Cache-key reuse doesn't imply byte-identical vectors |

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
- Same document, § Hand-reviewed contract checks: the literal W1 example (`toolVersion:1`), the
  modern sequence that lists six catalog entries, and W8's "on each of the six known tools". The
  historical W10 original-five check stays historical and is not rewritten.
- The #24 status line distinguishes the five *implemented* tools from the sixth *specified* by
  Decision 0012. The amendment keeps that distinction: search is *specified* until it ships.
- `local-topology.md` § T07: "The five cached read-only MCP tools, in order, are …". This sentence
  already predates Decision 0012's sixth specified tool. `local-topology.md`'s status line counts
  five *implemented* tools; it changes only when implementation makes a new count true.
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
- **Data:** `{hits:SearchHit[], documents:DocumentRef[], coverage:Coverage[],
  scopeCoverage:LanguageStatus[], provenance:Provenance[], searchIndex:SearchIndexStatus}`.
  - `SearchHit` is W2's `Symbol` (with `doc`, `range`, `lines`, `role`, `displayKey` and the syntax
    `Evidence` of the declaration itself) plus `rank:UInt` and
    `matchedBy:("lexical"|"vector")[]`. `displayKey` is output-only presentation, as in W2.
  - **Closure, exactly as W2 (§ W2 bullets; § Shared schema, "Find coverage contains the native
    tuples of the documents of the emitted symbols only"):** `documents` is the emitted-only
    document table; `coverage` is the sorted, deduplicated native coverage closure of the
    **emitted** hits only; `provenance` is the complete native closure for those hits. Zero emitted
    hits means `documents`, `coverage` and `provenance` are all empty. Missing required rows,
    corrupt digests or mixed revisions are `store_unavailable`, never best-effort.
  - **Searched scope, exactly as W2:** `scopeCoverage` has the four language rows counted over all
    native tuples in the answering snapshot. Any selected `partial` or `failed` tuple anywhere in
    the searched scope makes the result `partial:true` and adds one aggregate
    `coverageIncomplete/null` warning before `syntaxOnly`, **including on zero hits** and with
    healthy hits elsewhere. `syntaxOnly/null` is always present. Unselected-only tuples do
    neither, and zero hits never prove absence.
  - **Clipping, exactly as W2:** emit the longest fitting ordered hit prefix and recompute its
    document table, closure and warnings. Hits beyond `limit` set `truncated:true` with
    `truncationReason:"limit"`; hits dropped for bytes set `"response_bytes"`; a first hit that
    can't fit gives an empty truncated prefix; only a fixed empty envelope that can't fit is
    `range_too_large`.
- **Ordering (deterministic).** Within the answering snapshot:
  - **Candidates** are eligible named declarations of the candidate kinds, one per
    `(document version, symbolId)`; a declaration appears at most once.
  - **Lexical arm:** FTS5 BM25 over the split-identifier table, ordered by BM25 score ascending,
    then `symbolId` bytes; cut at depth `D`. Lexical rank `l` is 1-based. The MATCH expression is
    built from the query by the transformation below, and BM25 is scored over the **head-only**
    corpus (§5), so its statistics are a function of the answering head alone.
  - **Vector arm:** cosine similarity of the query vector to each candidate that has a current
    vector, ordered by similarity descending, then `symbolId` bytes; cut at depth `D`. Vector rank
    `v` is 1-based.
  - **Fusion:** score = `wL/(k+l) + wV/(k+v)`, where an arm that didn't return the candidate
    contributes 0. Order by score descending, then by the smaller of `l` and `v`, then `l`
    (absent last), then `symbolId` bytes. The constants (`k`, `D`, `wL`, `wV`) are fixed in the
    contract. The first-scope proposal is `k=60`, `D=100`, `wL=wV=1`; the #211 benchmark shows
    1:1 needs tuning (see open question 5), so these are placeholders for the owner.
  - `rank` is the 1-based position in the emitted list, contiguous. `matchedBy` is nonempty, unique,
    in the fixed order `lexical` then `vector`, and lists the arms that returned the candidate.
  - **One cache snapshot per call.** The vector arm and `searchIndex` counts read `vectors.db` in
    one read transaction, so within a call the candidates, `matchedBy`, `embedded` and the order
    are coherent with each other.
  - **What the basis promises, and what it doesn't.** `evidenceBasis` pins the **index**
    snapshot: candidates, card hashes, lexical scores, closure and coverage. It does **not** pin
    `vectors.db`, which is external, filled asynchronously and evictable. Two calls at the same
    basis can therefore return different vector candidates, `rank`, `matchedBy` and `embedded`
    counts while the background job fills the cache or eviction removes entries.
  - **Proposed default: no cross-call equality promise.** Two calls, even at the same basis and
    with the same request, may rank differently. The cache can fill or evict entries, and a vector
    re-embedded for the same card hash can have different bytes. Within one call, the ordering
    rules above apply to the vectors that call read. `searchIndex.embedded` and `state` let a
    caller see that the cache was incomplete.
  - **A cross-call guarantee is an open question (6).** The owner may later ask for a cross-call
    determinism guarantee: that repeating a call reproduces its ranking. If so, its exact
    definition would be specified and reviewed then: the ranking comparator, the numerical rules,
    the build and runtime identities it depends on, and whether it covers the pre-clipping order
    or the emitted hits. This proposal specifies none of that. Nothing in the default path depends
    on it.
- **Query transformation (deterministic and total).** Any valid 1–1024-byte query maps to a
  lexical arm as follows. Raw caller text never reaches FTS5's query parser.
  - **Tokenizer, fixed by the contract.** The search table uses `tokenize = "porter unicode61
    remove_diacritics 2"`, with `unicode61`'s default `categories "L* N* Co"` and no `tokenchars` or
    `separators` options. Under those defaults, the token characters are the Unicode general
    categories L\* (letters), N\* (numbers) and Co (private use). Every other character is a
    separator, **including `_`**. Trellis applies the same splitter (steps 1–2) to card text before
    inserting it and to the query. So every term Trellis emits is exactly one `unicode61` token on
    both sides, and FTS5's own case folding, diacritic removal and porter stemming then apply
    identically to both.

  The steps:
  1. Take maximal runs of `unicode61` token characters (L\*, N\*, Co). Everything else is a
     separator, including `_`, quotes, `*`, `^`, `:`, `(`, `)`, `+`, `-` and whitespace. So
     `read_revision_for` gives three runs, `__` gives none, and `readRevisionFor` gives one.
  2. Split each run at camelCase and letter–digit boundaries. Keep the whole run and its parts.
     Drop terms shorter than 2 characters and those on a fixed, versioned stopword list.
     Deduplicate in first-occurrence order, and keep at most 32 terms. Each term consists only of
     token characters, so it is exactly one FTS5 token; a term can't tokenize to nothing.
  3. Emit each term as an FTS5 string literal: `"` + term + `"`. Terms contain no `"`, and any
     `"` would be doubled anyway. Join them with ` OR `, and bind the expression as a SQL
     parameter to `MATCH`. A quoted string literal is never an FTS5 operator or column filter, so
     the words `AND`, `OR`, `NOT` and `NEAR` inside the query are plain terms.
  4. If no term remains (punctuation-only, underscore-only or stopword-only input), **the lexical
     arm is empty**, and no FTS5 query runs. The result comes from the vector arm alone, or has
     zero hits. This is the only query-shape fallback; it is decided before FTS5 is touched.
  5. **Errors after FTS5 runs are never a fallback.** An expression built this way can't be a
     syntax error: terms contain only L\*/N\*/Co characters, so no NUL or quote ever reaches the
     parser. Errors from running it are classified like every other Trellis read (see **Error
     classes** below). None of them is answered by retrying with raw text, or by a vector-only
     result presented as if the lexical arm had succeeded.

  The W2 trigram prefilter (§11) has its own total rule:
  - **Query limitations, decided before FTS5.** The prefilter is used only for a *representable*
    query: one with at least three Unicode characters and no U+0000. W2 admits U+0000 in its
    literal query, but an FTS5 string literal can't carry it (FTS5's parser ends the string at the
    NUL and reports `unterminated string` on a healthy table). Every other query uses W2's
    literal scan, decided from the query alone. A representable query becomes one FTS5 string
    literal with `"` doubled.
  - **After FTS5 runs,** errors follow the error classes below. They are never answered by a
    successful literal-scan fallback.

  **Error classes** (both FTS5 paths, and the vector-cache read):
  - **Contention is not an invalid store.** `SQLITE_BUSY` or `SQLITE_LOCKED` from another Trellis
    process or connection follows the existing rule (CLAUDE.md, AGENTS.md): it becomes the
    internal typed `storage_busy`, with a bounded wait and retry inside the call. If contention
    outlasts the call's five-second deadline, the call returns W8's existing retryable
    `deadline_exceeded`, as Trellis already maps exhausted read contention for MCP. Cancellation
    suppresses output as usual. No new wire code is added.
  - **`store_unavailable` is for actual store faults only:** corruption (`SQLITE_CORRUPT`,
    `SQLITE_NOTADB`, a malformed FTS5 index), a schema mismatch, and I/O, permission or
    unsafe-file errors. These are W8's nonretryable "unsafe/unreadable/invalid store", per T00.
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
5. **Fusion constants.** Plain 1:1 RRF fixed error-message lookups but cost the best model
   precision on the benchmark (#211). Should the contract fix a tuned rule instead, for example a
   lexical arm limited to exact phrase, literal and identifier matches? The ordering rule above
   stays deterministic whichever constants are chosen.
6. **Cross-call determinism.** The default makes no cross-call equality promise. Does the owner
   want a cross-call determinism guarantee later? If so, its exact definition would be specified
   and reviewed then: the ranking comparator, numerical rules, build and runtime identities, and
   pre-clipping versus emitted scope.

## 2. Card content and native extraction

**Touches.**

- `semantic-search-plan.md` § What gets embedded (unratified).
- `contract-v1.md` § Records and bindings: the closed `Declaration` record.
- `local-topology.md` § T05: "Publish graph, source bytes, input inventory, dependency
  observations and generation/revision atomically."
- Decision 0012 § Compatibility, as the precedent: "Change the native producer version and rebuild
  the index; do not migrate it."

**Touches also.**

- `contract-v1.md` § Stable identifiers: "Display keys are presentation only: they are never
  identity, never accepted as input, and never hashed."
- `mcp-readonly-pilot-contract.md` § Shared schema (qualified `displayKey`): "is never parsed as an
  ID, and never enters requests, hashes, lookup, ranking, ordering, joins, resolution or
  completeness."

**Proposed change.** A card is built from **separate measured native fields**, never from the
`displayKey` presentation label, and it never reproduces the `path#A.B.name` label format. It
holds, as separately labelled fields:

- the declaration's measured `name` and `kind`;
- the measured names of its named ancestors (outermost first), and, for a Rust `impl` ancestor, the
  measured impl self type and trait (a new search-only fact; extraction stores impl blocks
  unnamed today);
- the root-relative `path`, as its own field;
- the signature, from the stored header, parameters and header items;
- the doc comment;
- body facts: callee spellings (from `native_version_calls`), types named in the signature and
  body, string literals, error-code-shaped tokens in literals, and member names (for containers);
- for card variant (c) only, a capped head of the declaration's source.

Because the card hash and the ranking inputs are these measured fields, neither rule above is
touched: `displayKey` is still computed only for output. Native extraction stores headers,
parameters and call spellings today. It does **not** store doc comments, string literals, body
type names or the impl self type. The proposal is:

- capture those four during native extraction, from the same tree-sitter tree and the captured
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

7. Should the search-only facts live in `index.db` beside the declaration rows, or should they
   be computed only when a card is built, and never stored? Storing them costs index size (#199);
   not storing them means a rebuild of the vector cache needs the tree again.
8. Should the card format be versioned separately from the native producer version
   (`cardFormat` in `modelId`, §6)? This proposal says yes, so a card-format change re-embeds
   without re-extracting.

## 3. Search-index status, separate from #22 coverage, freshness and the result closure

**Touches.**

- `contract-v1.md` § Identity and coverage: "`complete` means all requested, supported evidence
  for that document."
- `contract-v1.md` § Basis and freshness: "Freshness is independent from coverage." The states are
  `fresh | possiblyStale | stale`, with "no numeric confidence".
- `contract-v1.md` § warnings-v1: the closed warning codes.
- `mcp-readonly-pilot-contract.md` § Result completeness: "Find/declaration/outgoing `partial` iff
  truncated, returned selected coverage incomplete, or a returned boundary call."
- `mcp-readonly-pilot-contract.md` § W2, **Completeness**: "any selected partial or failed tuple
  anywhere in the searched scope (from `scopeCoverage`) makes find `partial:true` and adds one
  aggregate `coverageIncomplete` warning before `syntaxOnly`, even on zero hits".
- `mcp-readonly-pilot-contract.md` § Shared schema: the closed `Warning.code` enum.

**Proposed change.** Add a closed wire type, used only by the search tool:

```text
SearchIndexStatus = {
  state: "complete" | "building" | "partial" | "unavailable",
  modelId: Text | null,     // hex modelId (§6); null when unavailable
  modelName: Text | null,   // the pinned model's name, e.g. "voyage-4-nano"; null when unavailable
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
- **Native evidence completeness is unchanged and still reported.** Coverage tuples, freshness,
  `Evidence`, warnings-v1, the emitted-hit closure and W2's searched-scope rule apply exactly as in
  §1: incomplete *native extraction* sets `partial:true` and `coverageIncomplete`, with or without
  hits. A search hit's `Evidence` describes the declaration's own syntax record (as in W2), not the
  hit's relevance.
- **Vector incompleteness is separate.** Missing vectors appear **only** in `searchIndex`. They
  don't set `partial` and add no warning code, and they never hide incomplete native evidence.

This replaces the plan's `searchIndex.indexPin` and `embeddedPin` fields. Vectors are content
addressed (§5), so "embedded as of revision N" isn't meaningful. What matters is how many
declarations *at the answering snapshot* have a vector, which the counts state directly. The
envelope's `evidenceBasis` already names the snapshot.

**Why.** #22's coverage says whether *evidence* was extracted. Whether a background vector job has
finished is a different fact. Reusing coverage, `partial` or a warning for it would make every
search during a rebuild look like an evidence failure, and would weaken those signals for the
other tools.

**Open questions.**

9. Should vector incompleteness also set the envelope's `partial:true`? This proposal says no,
   because `partial` is about evidence. An agent that wants complete vector coverage reads
   `searchIndex.state`.
10. With no model configured, should the tool answer lexical-only (proposed), or fail? Answering
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
  - Decision 0006 § 1 retains head plus revisions superseded less than 15 minutes ago, and makes a
    pin to a *released* revision conflict. Retention governs what the index keeps; it does not by
    itself make a retained revision an MCP-eligible snapshot.
  - `mcp-readonly-pilot-contract.md` § W8: evidence reads admit "a committed, validated head";
    "Startup reconciliation commits a new validated head | Subsequent reads can use the new
    coherent basis; a complete pin to the old basis conflicts." § Result completeness: "stale
    complete pin conflicts before not_found".

**Proposed change.** Search follows W8 as it stands, with no new conflict class.

- **One snapshot.** One eligible snapshot, the validated head admitted at read admission, answers
  the whole query: lexical candidates, the card hashes of eligible declarations, and the
  `documents`, `coverage`, `provenance` and `scopeCoverage` rows.
- **Pins.** With `expectedBasis`, the call compares it with that admitted head and returns
  `revision_conflict` when they differ. It never reads an older retained revision. Without it,
  the answer uses the admitted head and reports it in `evidenceBasis`.
- **Follow-up reads may conflict.** An agent that passes the returned basis to
  `trellis_declaration` gets a `revision_conflict` if a new head was published in between. It then
  searches again at the new head. Search does not promise that its hits stay verifiable at its
  basis.
- **Vectors.** Vectors are looked up by the card hash stored *in that snapshot*. A vector embedded
  for an identical card at another revision is reused by its cache key `(modelId, cardHash)`, so no
  cross-revision evidence is involved. Reusing a key does not mean the bytes are identical to a
  past or future embedding of that card: a re-embedding after eviction, or under another runtime,
  may differ (§1). A missing vector only lowers `embedded` (§3).
- **Ranking.** The ranking is global within that snapshot. It is never stitched across revisions,
  and a later publication never moves an in-flight answer.

**Why.** Search returns IDs that the agent then passes to `trellis_declaration` with the same
`expectedBasis`. Using W8's existing head-only rule keeps that pair consistent or makes it fail
typed, and needs no new rule; the plan's citation should be fixed.

**Open questions.**

11. Should search *require* `expectedBasis`? This proposal says no: it is optional, as in W2,
    because the first search usually starts a session.
12. Should MCP reads (search, and then declaration) be able to select a *retained* older
    revision, so a search-then-verify sequence survives a publication? That would be a separate
    #24 pin-eligibility amendment for all evidence tools, not part of this proposal.

## 5. Where vectors live, and their invalidation

**Touches.**

- `local-topology.md` § T02, the fixed-locations table. Its rows are the derived index, use lock,
  explicit requests, optional facts (`<cache>/facts.db`), and tokens. Retired record and ledger
  paths remain ignored under #305.
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
- **`index.db`** holds:
  - each declaration's `cardHash` and the search-only card facts, per document version, so #67's
    version sharing applies;
  - a **head-only** split-identifier FTS5 table. It holds one row per candidate declaration of
    the current head, and nothing for retained or released revisions. The publication that
    advances the head updates it in the same transaction: delete the rows of document versions
    leaving the head, insert those entering it. FTS5's BM25 corpus statistics (row count,
    average length, document frequencies) are therefore a function of the head's rows alone.
    They don't drift when older revisions are retained, released or collected, and a retained
    revision's rows can't affect scoring. This fits W8, which admits only the head (§4). The
    table is derived; a rebuild repopulates it from the head.

  `cardHash` is SHA-256 over a new domain, `trellis.search-card.v1\0`, and the exact card bytes.
  Vectors are never stored in `index.db`.

  The alternative, per-version FTS rows filtered to the head with custom corpus statistics, would
  need a ranking function written outside FTS5's `bm25()`. It is not proposed.
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
- **Corruption (T00).** Two cases, kept apart:
  - **A row that fails typed decoding.** For example, the blob length isn't `dimensions × bytes
    per component`, or its `modelId` doesn't match the key. This is an **owner decision (open
    question 14), not settled by an existing rule.** The two rules in play:
    - T00, "Accidental corruption of state files": "rows that fail typed decoding or closed
      validation return the contract's typed corrupt failure, never a partial or best-effort
      result";
    - T02, for the optional fact cache: "a missing, corrupt, conflicting or unavailable entry falls
      back to extraction".

    The options:
    - **Option A, T00 as written:** the search call fails `store_unavailable`. No amendment is
      needed. A single bad row makes search unavailable until the daemon repairs or recreates
      the cache.
    - **Option B, a narrow amendment** extending T02's fact-cache rule to the vector cache: a
      vector-cache row that fails typed decoding is a **specified miss**. That candidate is
      lexical-only, counts against `embedded`, and is re-embedded in the background. This would be
      an explicit T00 exception, limited to disposable optional vector-cache entries whose miss
      changes ranking but never evidence. The new T02 row and T00 would both say so.

    This proposal leans to option B, by analogy with facts, but makes no claim that it is already
    allowed.
  - **A database-level failure** (SQLite integrity failure, a malformed schema, or an unreadable or
    unsafe file) fails the search call with the typed `store_unavailable` and no hits. T00 requires
    "the contract's typed corrupt failure, never a partial or best-effort result", and #24 W8 says an
    "unsafe/unreadable/invalid store" returns `store_unavailable`. A lexical-only success here would
    be a best-effort answer. The daemon may then recreate `vectors.db` as disposable derived state,
    outside the read.
  - **Contention is neither case.** `SQLITE_BUSY` or `SQLITE_LOCKED` on `vectors.db` (for example
    while the embedding job writes) is the internal `storage_busy`, with a bounded wait and retry,
    then W8's retryable `deadline_exceeded` (§1, error classes). It is never `store_unavailable`
    and never a lexical-only success.
  - **Alternative, for the owner:** classify `vectors.db` as wholly optional, so that a
    database-level failure answers lexical-only with `searchIndex.state:"unavailable"`. That keeps
    search available on a damaged cache, but it is an explicit exception to T00's corrupt-failure
    rule and to W8, and it would need amendments to both. This proposal does not recommend it.
  - No per-vector checksum: under T00 and CLAUDE.md, Trellis trusts what it wrote and validated.
    This narrows the plan's proposed vector-blob checksum.

**Why.** A vector is keyed by `(modelId, card text)`, and any vector stored under that key is a
valid embedding of that card for that model. Sharing them per user, like facts, avoids
re-embedding for each worktree or rebuild. The key identifies *what* was embedded, not
byte-identical output: two embeddings of one key on different runtimes may differ in the last bits
(§1). Keeping them out of `index.db` avoids
adding to the size problem #199 is working on.

**Open questions.**

13. Per-user cache (proposed), or `index.db`? `index.db` would make vectors part of the atomic
    publication, but it would multiply storage by worktrees, and a rebuild would drop them.
14. **Corrupt vectors.** For a vector-cache *row* that fails typed decoding: option A (T00 as
    written, `store_unavailable`) or option B (a narrow T00/T02 amendment making it a specified
    miss)? For a *database-level* failure: typed `store_unavailable` (proposed), or a lexical-only
    fallback with explicit T00 and W8 exceptions?
15. What should the default cache cap be, and should it be configurable? See §8 for sizes.
16. Should `trellis gc --report` list `vectors.db` size and entry count? It lists derived indexes
    today, and facts are not listed.

## 6. Model provenance pinning

**Touches.**

- `contract-v1.md` § Identity and coverage: `Producer.id`, `.version`, `.executableHash`.
- `semantic-search-plan.md` § Model: "The model is pinned like a producer".
- `local-topology.md` § T02: "Tokens | Explicitly configured external paths".

**Proposed change.**

- **`modelId`.** It is SHA-256 over a domain `trellis.search-model.v1\0` and the canonical JSON of
  `{name, revision, weightsSha256, tokenizerSha256, dimensions, quantization, maxTokens,
  queryPrompt, documentPrompt, pooling, cardFormat}`.
  - `revision` is the upstream commit, for example the Hugging Face commit hash.
  - `weightsSha256` covers every weight file, in sorted-name order.

  The wire reports the hex `modelId` and the name in `SearchIndexStatus.modelId` and
  `.modelName` (§3).
- **Unknown models are refused.** Trellis ships a closed table of supported `(name, revision,
  weightsSha256)` triples. A configured model whose hashes don't match is reported as
  `searchIndex.state:"unavailable"`. It is never loaded under another identity.
- **Weights.** They come from an explicitly configured path. This is a new proposed placement:
  T02's configured external paths cover tokens today, not model weights. Trellis
  never downloads them (D8). The weights are hashed once when the daemon loads the model, then
  pinned for that daemon's lifetime.
- **Not a producer.** The model is not a #22 producer and gets no `Producer` row, because its
  output is not evidence (§7). Its pin exists for cache correctness and reproducibility.

**Why.** Vectors are only comparable within one exact model, prompt and card format. A silent
weight or prompt change would mix incompatible vectors in one ranking.

**Open questions.**

17. Should Trellis bundle a default model, or require the user to configure one? Bundling makes
    search work out of the box but adds hundreds of megabytes to the distribution. Configuring
    keeps the binary small.
18. Which inference runtime: ONNX Runtime or candle? Both run in-process. ONNX Runtime adds a
    native library, and candle adds build time. The benchmark used PyTorch and ONNX Runtime only
    to measure quality. A Rust runtime's speed still needs its own measurement.
19. Some candidate models (voyage-4-nano, jina-embeddings-v2-base-code) publish custom Python
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
- the wire carries **no numeric similarity score**, only `rank` and `matchedBy`. This is a
  proposed product choice, not a #22 requirement: #22's "no numeric confidence" is about evidence
  freshness. Scores are model-specific and invite reading as confidence;
- zero hits, or a missing expected hit, proves nothing about absence;
- search results never feed #22 records, durable anchors, anchor reattachment, Jev or ACP
  packets, or any other tool's evidence. Agents verify a hit with `trellis_declaration` and
  `trellis_outgoing_calls` at the returned basis, which may conflict after a new head (§4);
- the plan's optional Jev rerank, if ever added, follows the same rule. Its labels are retrieval
  metadata.

**Why.** The core #22 guarantee is that evidence is measured or produced with provenance, never
inferred. Similarity is inference.

**Open question.**

20. Should hits expose scores for debugging behind a non-default flag? This proposal says no.

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
  callees and literals) should be smaller. It must be measured against #199's numbers when built.
- **Embedding time** is a background cost, not an indexing cost. See #211 for measured
  full-repository times per model.

**Why.** Vectors could otherwise grow the index #199 is shrinking. A per-user cache with a fixed
cap bounds them independently of the number of checkouts.

**Open questions.**

21. Should search wait for #199's size items before shipping? #211 sequences it after them, but no
    ratified record makes that a release gate; it is an owner decision.
22. Should a full-dimension mode (2048 for voyage-4-nano) be allowed? The benchmark shows what
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
- **Accidental corruption.** A database-level failure is the typed `store_unavailable`, never a
  best-effort answer. Whether a single row failing typed decoding is a typed failure (T00 as
  written) or a specified miss (a narrow amendment) is an owner decision (§5, open question 14).

Out of scope, per T00: signing or authenticating cached vectors against a same-user process.

## 10. Remote embedding arm

**Touches.**

- `mcp-readonly-pilot-contract.md` opening: tools never "invoke a provider".
- `semantic-search-plan.md` § Model: "A remote embedding API is available only as an explicit
  opt-in."
- The retired live Jev runtime is not a provider or accounting contract for future search.
  A remote provider would need separate owner approval and an explicit accounting contract.

**Proposed change.** None now. voyage-code-4 sends repository code, and query text, to a hosted
service. It is **out of scope** until the owner opts in. It would then need:

- its own amendment, because embedding a query at read time would be a provider call;
- a key from the environment;
- a ledger;
- a separate `modelId`.

The #211 benchmark did not run it.

**Open question.**

23. Does the owner want the remote arm measured at all, and on which repository?

## 11. Coordination with #199 item 5 (Q-8, symbol search speed)

**Status:** the W2 part below is ratified by [Decision 0013](semantic-evidence/decisions/0013-symbol-search-trigram-prefilter.md) (2026-10-20). The search-table part stays proposed with #211.

**Touches.**

- `mcp-readonly-pilot-contract.md` § W2: "literal name/ID substring … Use SQLite built-in ASCII
  `lower` and literal `instr`."
- #199 item 5: "Symbol search: a narrow table or trigram index (Q-8)."

**Proposed change.** None to W2. Two FTS5 tables. The W2 prefilter may be per document version,
because it only filters candidates and never scores. The search table is head-only, because it
scores (§5).

- **For W2:** a `trigram` table used only as a **candidate prefilter**. FTS5 trigrams are built
  from Unicode **characters**, not UTF-8 bytes, so the prefilter is eligible only when the query has
  at least three Unicode characters. For example, `éa` is three UTF-8 bytes but two characters and
  forms no trigram. Index an ASCII-`lower`ed copy of the name and ID with `case_sensitive 1`, so
  trigram folding can't diverge from W2's ASCII-only `lower`. Every prefiltered candidate is still
  checked with W2's literal ASCII-`lower`/`instr` rule. The query is ASCII-`lower`ed the same way
  before it is quoted.
  - **Literal-scan fallback, for query limitations only.** It is decided from the query alone,
    before FTS5 runs. A query takes the literal scan when it has fewer than three Unicode
    characters (no trigram) or contains U+0000 (an FTS5 string literal can't carry a NUL).
  - **After FTS5 runs,** errors follow §1's error classes and never fall back. Contention
    (`SQLITE_BUSY`/`LOCKED`) is the internal `storage_busy`, retried within the deadline, and then
    `deadline_exceeded`. Actual corruption, schema mismatch and I/O faults are
    `store_unavailable`.
  - W2's output stays byte-identical for **all** valid 1–256-byte queries that it answers. Its
    256-byte query limit is unchanged; it is a byte limit, independent of trigram eligibility.
- **For search:** a head-only table with tokenizer `porter unicode61 remove_diacritics 2`
  (default token characters, so `_` is a separator). It holds the card's measured fields after
  Trellis's camelCase split, and is queried only through the transformation in §1.

One table can't serve both semantics exactly, so this proposal doesn't share one.

**Open question.**

24. Is one shared table worth W2's exactness risk? This proposal says no: two tables.
