# Decision 0013: trigram candidate prefilter for symbol search

- **Status:** owner-ratified 2026-10-20 on [#199](https://github.com/SquashMerge/trellis/issues/199) (item 5, Q-8). Three rulings, all recorded on #199 and all recorded here:
  - The first approves the W2 part of D12 in [`../../semantic-search-contract-proposal.md`](../../semantic-search-contract-proposal.md) §11 as proposed.
  - The [names-only ruling](https://github.com/SquashMerge/trellis/issues/199#issuecomment-6297417762) narrows the table to symbol names, with exact results kept for queries that can match IDs.
  - The [second-table ruling](https://github.com/SquashMerge/trellis/issues/199#issuecomment-6297450336) keeps `graph_node_trigrams` for the browser and CLI search. Decision [0016](0016-graph-rows-derived-from-native-facts.md) narrows the underlying `graph_nodes` to names, IDs and kinds without changing this ruling.

  The number 0013 was last used by the closed, unmerged WAL draft (#215), which never became a record.
- **Amends:** nothing in W2's output. [`../../mcp-readonly-pilot-contract.md`](../../mcp-readonly-pilot-contract.md) § W2 keeps its literal ASCII-`lower`/`instr` rule, ranking, limits and truncation. This record adds only how candidates are found.
- **Not decided here:** the search-side table and the rest of D1–D13, which stay with [#211](https://github.com/SquashMerge/trellis/issues/211). Whether one table could serve both stays open.
- **Compatibility:** none needed for disposable derived indexes. The new tables change the index format, so the index schema version changes and existing indexes are recreated, not migrated.

## Decision

1. **The tables.** Each FTS5 table uses `tokenize='trigram case_sensitive 1'` and holds an ASCII-`lower`ed copy of each symbol's **name only**. IDs are not indexed.
   - The copy is written with SQLite's own `lower`, the same function the literal rule uses, so trigram folding can't diverge from it.
   - A table is only a candidate prefilter: it never scores or orders.
   - It may hold every retained document version. The usual manifest join selects the answering revision.
   - There are two tables: `native_declaration_trigrams` for W2, and `graph_node_trigrams` for the browser `/api/symbols` and CLI `symbols` search. That search runs the same literal rule over the narrow `graph_nodes` name/ID table. Keeping the second table is an owner ruling (see Status).
2. **The query.** A query is ASCII-`lower`ed the same way, then quoted as one FTS5 string literal with `"` doubled. FTS5 operators, column filters and parentheses in the query are therefore plain text.
3. **Queries that can match an ID**, decided from the query alone, before FTS5 runs.
   - **The ID format.** Every stored ID, native and graph, is a minted SyntaxId: `sid:v1:` followed by 32 lowercase hex digits.
   - **The rule.** `lower(q)` can occur in such an ID exactly when it is one of these:
     - a substring of `sid:v1:`; or
     - a suffix of `sid:v1:` (possibly empty) followed by 1–32 hex digits.

     So all-hex queries such as `f0001` or `add` are ID-shaped. Queries with any other letter or punctuation are not, for example `parseConfig`, `peerValue` or `f0001x`. Neither is anything longer than 39 characters.
   - **Candidates.** An ID-shaped query's candidates are the name-table matches plus the rows a covering scan of the ID column finds in the answering revision (`UNION`, so a row that matches through both its name and its ID appears once). Any other query's candidates are the name-table matches alone. That set is exact for it, because no ID can contain the query.
4. **Verification.** Every candidate is still checked with W2's literal ASCII-`lower`/`instr` rule on the name **or** the ID, and ranked, tie-broken and limited by the same SQL as before. A query that matches through both a name and an ID therefore gets the literal scan's hits, order and LIMIT.
5. **The ID-scan shortcut, which is not a fallback.**
   - **When it applies.** The scan is skipped when the name candidates alone already give `LIMIT` hits (W2's `limit+1` lookahead; the browser's `limit`) and every one of them is an exact-name or name-prefix hit.
   - **Why the answer is unchanged.** A row only the ID scan adds matches through its ID alone. Its name neither equals nor starts with the query, so it ranks 2, after every rank-0 or rank-1 hit. The output order is rank first, so every such row would fall beyond the LIMIT.
   - **Why it is not a fallback.** It never answers from a different path after an error or a limitation. It runs only after the names query has succeeded, and it only skips a second query that provably can't change the answer.
6. **Literal-scan fallback, from the query alone, before FTS5 runs.** These queries take W2's literal scan:
   - a query with fewer than three Unicode characters, which forms no trigram (`éa` is three bytes but two characters);
   - a query containing U+0000, which an FTS5 string literal can't carry.
7. **Errors after FTS5 runs never fall back.** SQLite `BUSY`/`LOCKED` is the internal `storage_busy`, retried within the call's deadline, then W8's `deadline_exceeded`. Corruption (including a malformed FTS5 index), schema mismatch and I/O faults are `store_unavailable`.
8. **Output.** W2's output stays byte-identical for every valid 1–256-byte query: the same hits, order, truncation and limits. Its 256-byte query limit is unchanged. The browser and CLI output stays byte-identical in the same way.

## Implementation notes

- **Storage and maintenance:**
  - Both tables are contentless (`content=''`, `columnsize=0`) and keyed by the source row's `rowid`.
  - Triggers on the source table keep them in step on insert, delete and name update, so every existing publication, retention and rebaseline path maintains them.
  - A delete uses FTS5's `'delete'` command with the same `lower`ed name the insert wrote. `contentless_delete=1` was measured about 7× slower to insert at 10k.
- **FTS5 settings:** phrase matching needs `detail=full`. `detail=none` and `detail=column` reject a multi-trigram string.
- **The ID scan:** it reads an existing covering index, such as the primary keys `(version_id, syntax_id)` and `(projection_id, id)`; SQLite may pick another covering index, for example `graph_nodes_name`. Either way it never reads the wide rows. Indexing IDs as trigrams cost about 95% of the tables' size, because each ID adds about 37 near-unique trigrams.
- **Not a search index:** neither table is a search index for #211.
