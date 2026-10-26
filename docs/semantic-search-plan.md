# Semantic symbol and code search — plan

Status: **proposed, not ratified**. This document is a design direction. It does not govern any
issue, authorize implementation or a Feature Factory run, or amend the
[semantic evidence contract](semantic-evidence/contract-v1.md), the
[local topology](local-topology.md) or the [MCP contract](mcp-readonly-pilot-contract.md). The
contract changes it implies (see [Contract fit](#contract-fit)) need their own review when the work
is scheduled.

The owner’s #211 D1–D13 rulings and later runtime/backend rulings are recorded in [Decision 0018](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md), [#23](local-topology.md) and [#24](mcp-readonly-pilot-contract.md). Those records govern search; this plan/proposal is not ratified wholesale. Historical alternatives and counts below are not current implementation authority.

## Goal

Let agents and people find code by meaning — "where do we validate the pin?", "code that retries on
SQLite BUSY" — and land on an exact, verifiable declaration. For agents this replaces the
exploratory grep loop that usually precedes any change.

Search results are **retrieval, not evidence**. A hit is a candidate to inspect, never a semantic
fact.

## What gets embedded

Chunks are built during native extraction from the tree-sitter tree and the already captured
bytes. There is no second parse and no file reread.

Two document kinds:

1. **Symbol cards.** A qualified name, kind, signature, doc comment and path, optionally with the
   names of callers and callees for disambiguation (off at first; see
   [Incremental updates](#incremental-updates)). Cards match "which function does X" queries.
2. **Code chunks.** The declaration's source, so undocumented or misleadingly named code is still
   found.

### AST chunking

- The base unit is a function, method or class node, with its exact byte range.
- An oversized body (over a token budget of roughly 512–1024) is split recursively along its
  child nodes (statements, blocks, inner functions), then small adjacent pieces are merged back up
  to the budget. Chunks stay syntactically whole; a loop is never cut in half.
- Every chunk carries a breadcrumb header, for example
  `src/store.rs › impl Store › fn declarations_for_anchor(...)`, so a fragment is meaningful on its
  own.
- A class summary chunk holds the class signature plus member signatures, without bodies. A file
  chunk holds the imports and top-level statements.

### Trivial declarations

Getters, setters, pure delegations, empty or default constructors and derived or generated members
get **no code-chunk vector**. Their bodies look alike in embedding space and crowd real hits out of
the top-k. They remain findable: their signatures are in the class summary chunk and their names
are in the lexical index.

Triviality is scored from the syntax tree: statement and node counts, branch count, and
recognizable shapes. Those shapes are a single `return` of a field, parameter or constant; a single
field assignment; a single call that forwards its arguments; an empty or `super(...)` body; and a
`#[derive]`-style or generated member. The threshold is tuned by measuring recall@k with and
without the filter, not guessed.

### Metadata

| Embedded in the chunk text | Stored beside the vector |
| --- | --- |
| File path | Line and column range, derived from the byte range at the pinned revision |
| AST breadcrumb and node kind | Byte range, stable declaration ID, revision, content hash |
| Signature and language | Ancestor IDs, depth, sibling ordinal |
| Optionally: imports used; caller and callee names (see fan-out note below) | Flags: test or production, visibility, `async`, generated or vendored |

Line numbers and ordinals are never embedded. They carry no meaning, and embedding them would break
the cache: one line added at the top of a file would change the text, and so the content hash, of
every chunk below it.

## Model

- **Local by default.** Code never leaves the machine. A small code-capable embedding model runs
  in-process via ONNX Runtime or candle, using Metal on the reference host. Local candidates are
  **voyage-4-nano** (open-weight, Apache 2.0; Matryoshka and int8/binary quantization; shares an
  embedding space with the hosted voyage-4 family), jina-embeddings-v2-base-code, nomic-embed-code
  and Qwen3-Embedding-0.6B. Choose one with a bake-off on real queries against this and other
  repositories.
- A remote embedding API is available only as an explicit opt-in. The hosted candidate is
  **voyage-code-4**. Do not assume it shares voyage-4's embedding space: switching between it and a
  local model re-embeds, which the `modelId` in the cache key already enforces.
- The model is **pinned like a producer**, and its pin is part of `modelId`, so vectors are tied to
  the model that produced them.
  - *Local models:* name, version and weight-file hash are recorded, and the vectors are
    reproducible.
  - *Hosted models:* weights can't be hashed. `modelId` records the provider, model name and the
    provider's model version or release identifier where one is published. The vectors are treated
    as reproducible only while that identifier is unchanged. A changed identifier, or a periodic
    drift check that re-embeds a small canary set and finds changed vectors, invalidates that
    `modelId`'s cache entries and re-embeds.

## Storage and ranking

- **Two stores.**
  - *Shared vector cache.* Vectors live in a per-user, content-addressed cache beside the native
    fact cache (#71's `<cache>/facts.db`), keyed by `(modelId, hash of the embedded text)`. They
    are int8-quantized blobs in SQLite.
  - *Per-index search tables.* Each checkout's index holds only references: chunk → text hash,
    plus stable declaration ID, byte range and flags. It also holds the lexical (FTS5) index.
- A brute-force cosine scan over the referenced vectors is enough at first: 100k chunks × 768
  dimensions is about 77 MB and takes milliseconds. Add an ANN index (for example `sqlite-vec` or
  HNSW) only if large repositories need it.
- **Hybrid ranking.** Vector similarity is fused with SQLite FTS5/BM25 over identifiers split at
  camelCase and snake_case. On code, exact identifier matches matter, and hybrid retrieval
  outperforms pure embeddings.
- **Filters:** language, path prefix, node kind, and excluding tests or generated code.
- **Boosts:** the same module as the caller's current file, public over private, production over
  test code.
- **Optional Jev rerank (opt-in, off by default).** Jev is a single-forward-pass relevance
  decision model evaluated in the historical selection research. The live Tools provider is retired;
  any future rerank needs a new provider/accounting contract. A rerank stage would send the query and the top ~50 hybrid candidates
  (symbol card plus a bounded code excerpt each) for one relevance judgment per candidate, and
  reorder by that judgment. This is the per-candidate judgment that
  [jevgrep](https://github.com/dzhng/jevgrep) applies across a whole folder → file → declaration
  crawl, limited here to candidates the local index already found.
  - It sends source to the hosted provider, so a new contract would require explicit opt-in, a
    `JEV_KEY` from the environment, a reservation budget ledger, and validated labels. Search
    without it stays fully local.
  - **Batch, and budget per query.** The retired live Jev contract reserved 10 cents per outbound attempt against a
    capped ledger (the documented cap is $5, so at most 50 attempts). One attempt per candidate
    would spend the whole cap on a single query, so that design is ruled out. The rerank sends
    **one batched attempt per query**, with all candidates in one bounded packet, labelled together.
    Candidates that don't fit the packet bounds are cut, never split across extra attempts. Search
    reranking gets its own ledger, separate from sequence-diagram selection, and shows the
    remaining budget. When the ledger can't cover an attempt, search uses the hybrid order and
    says so.
  - It reorders only. It never adds candidates, and a failed, over-budget or invalid judgment falls
    back to the hybrid order and says so in the response. Its labels are retrieval metadata, not
    evidence.
  - Include it only if the bake-off shows hybrid ranking alone falls short. Measure it by recall@k
    gain against its per-query cost.

## Incremental updates

- **Cache key: the full generated text.** A vector is keyed by a hash of the exact text that was
  embedded: breadcrumb, path, signature, body and any context fields. Unchanged text is reused
  across revisions, worktrees and checkouts. The cache is **not** path-neutral. The path is in the
  embedded text because it is a useful topic signal, so a rename or `git mv` re-embeds the
  affected chunks. Renames are rare enough that this trade favours retrieval quality.
- **New worktrees and repository copies reuse, never copy.** `index.db` is bound to its
  checkout's root identity and generation (local topology T01–T02, no state migration), so it is
  never copied between checkouts. A new worktree or copy of the same commit instead rebuilds from
  the shared caches:
  - native extraction hits the fact cache for every fact *still cached* (#71: "parses only files
    the cache has never seen");
  - every chunk's embedded text is identical, because embedded paths are relative to the root, so
    every vector *still cached* is a hit.

  The fast path, where nothing is parsed or re-embedded, holds only when **both** caches still
  hold every required entry. Then what remains is the capture's one read and hash per file, ID
  assembly and publication, which takes seconds, plus rebuilding the lexical index. Both caches are
  size-capped LRU, so entries may have been evicted. An evicted fact means that file is extracted
  again as usual. An evicted vector follows the cache-miss rule below: it is pending, served
  lexical-only, and re-embedded in the background.
- **Cache rules follow #71.**
  - Keep size-capped LRU eviction.
  - The key authenticates the *input*, not the stored vector. Each entry therefore also stores a
    checksum of its vector blob, together with the `modelId`, dimension and quantization, and is
    verified on read. An entry that fails verification counts as corrupt.
  - A missing, evicted, corrupt or unavailable entry is treated as *no vector* for that chunk.
    The chunk counts as pending in `searchIndex` (`partial` or `building`, `pendingChunks`), is
    served lexical-only until it is re-embedded, and is re-embedded in the background. Ranking can
    therefore differ until re-embedding finishes. The guarantee covers **detected** failures (a
    missing, evicted or checksum-failing entry): such an entry never yields a wrong vector, a wrong
    stable ID, or a response that claims complete vector coverage. Under
    [T00](local-topology.md#threat-model), a vector deliberately rewritten together with a matching
    checksum is out of scope.
  - Validate an entry against its key before use.
  - Never share the cache across users or machines.
- **Invalidate on text or context, not only on the declaration body.** A chunk's generated text
  can change while its declaration body does not. The incremental deltas (#67) must therefore
  select every chunk whose generated text would change:
  - a class summary chunk, when any member signature is added, removed or changed;
  - a file chunk, when imports or top-level statements change;
  - a symbol card, when its breadcrumb or signature changes, or, if caller and callee names are
    included, when those lists change.

  Regenerating a candidate chunk and comparing text hashes is the check: an equal hash keeps the
  vector.
- **Keep fan-out bounded.** Caller names change whenever a new call site appears elsewhere, so
  including them makes one edit invalidate cards in other files. Start without caller and callee
  names in the embedded text, and measure whether adding them is worth the invalidation cost.
  They are available to the lexical index and to boosts either way.
- **Background job.** Embedding runs after publication and never delays the index.

## Contract fit

- **Retrieval, not evidence.** A future `search_symbols(query, k, filters)` MCP tool returns stable
  declaration IDs with scores, byte and line ranges and breadcrumbs, labeled as retrieval. Agents
  verify through the exact `declaration` and `outgoing_calls` views. Nothing inferred by similarity
  becomes a #22 semantic record.
- **Search-index coverage is its own state.** The #22 evidence coverage and freshness fields
  describe evidence, not whether a background vector job has finished, and are not reused for
  that. Search responses carry a separate search-index status, for example:
  `searchIndex: { state: "complete" | "building" | "partial" | "unavailable", indexPin,
  embeddedPin, pendingChunks, modelId }`. `indexPin` and `embeddedPin` are full
  `{indexGeneration, indexRevision}` pairs (local topology T02), never bare revision numbers,
  because a revision number can recur after a rebuild or recreation.
  - `building` or `partial` means some chunks at the pinned revision have no current vector.
    Those candidates can still be returned from the lexical index, marked lexical-only.
  - `unavailable` means no vectors exist, for example when no model is configured. Search then
    degrades to lexical ranking and says so.
  - A response never implies that vector coverage is complete when it isn't.
- **Revision pinning.** The local topology already classifies "symbol search, ranked/global
  queries" as operations that conflict when stale. Search answers are pinned to one full
  `{indexGeneration, indexRevision}` pair and fail with a revision conflict when the pin is stale.
- **Catalog.** The first MCP release is closed at `declaration|outgoing_calls` (#17), so adding
  search is a versioned catalog extension (#24), like the later evidence views (#60).
- **Threat model.** This falls under [T00](local-topology.md#threat-model). Repository content is
  untrusted input to chunking and embedding, which must stay bounded. Deliberate same-user edits to
  stored vectors are out of scope.

## Sequencing

- After #67 (the durable queue and incremental publication), so updates are incremental from the
  start.
- Independent of #58 (SCIP import): chunks and cards come from the native syntax tier.
- Ideally in place before the planned agent benchmark (with and without Trellis). Exploration is
  where agents spend most of their search calls.

A likely split when scheduled:

1. Chunking, triviality scoring and metadata.
2. The embedding job, cache and vector storage, with hybrid ranking.
3. The `search_symbols` MCP catalog extension.

The model bake-off precedes item 2.

## Note: file change tracking and Merkle trees

This section records a design question that came up while planning incremental re-embedding.

**How changes are tracked.** Only capture is on `main` today; change detection and watching are
planned.

- **Capture (#64, on `main`).** Each admitted file is opened, read and SHA-256-hashed exactly once per index.
  The hash is stored with the file's size, mtime, ctime and inode.
- **Change detection (#70, T05; planned, in progress).** A full scan re-enumerates paths with the `ignore` walker and
  compares each file's size, mtime, ctime and inode against the stored table. It re-reads and
  re-hashes only on a difference or an uncertain ("racily clean") timestamp. An unchanged file
  costs one `stat`.
- **Watching (#16; planned).** A filesystem watcher (FSEvents or inotify) marks changed paths, with periodic
  full reconciliation as the safety net.

**A Merkle tree does not speed up change detection.** It is a structure for *comparing* hashes you
already have. Keeping its root current still requires the per-file stat and hash work above,
plus hashing up the directory levels. The effective speedups for detection are:

1. the watcher, whose work scales with changed files rather than total files;
2. optionally, a directory-listing cache keyed by directory mtime, which avoids re-enumerating
   unchanged directories (the `ignore` walk is the costly part of a scan, not the stats). This is
   like git's untracked cache. Content edits don't change a directory's mtime, so files are still
   stat'ed.

For scale, stat'ing 10k files on APFS takes tens of milliseconds, and 100k takes a few hundred.

**Where a Merkle tree does help: comparing snapshots.** A Merkle tree over the file hashes
*captured at each indexed revision* is cheap to build, because capture already computes every leaf.
It would provide:

- revision-to-revision diffs in O(changed × depth) rather than O(files), useful for #67 deltas
  and for deciding which chunks to re-embed;
- a single fingerprint for "is this the same tree?";
- reuse across worktrees, where identical subtrees have identical hashes. This complements the
  native per-file fact cache (#71).

It is optional and separate from change detection. Add it only if delta computation or
re-embedding shows a need.
