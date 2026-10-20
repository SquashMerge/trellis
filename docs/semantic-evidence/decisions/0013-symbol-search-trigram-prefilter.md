# Decision 0013: trigram candidate prefilter for symbol search

- **Status:** owner-ratified 2026-10-20 on [#199](https://github.com/SquashMerge/trellis/issues/199) (item 5, Q-8), approving the W2 part of D12 in [`../../semantic-search-contract-proposal.md`](../../semantic-search-contract-proposal.md) §11 as proposed. This records that ruling. The number 0013 was last used by the closed, unmerged WAL draft (#215), which never became a record.
- **Amends:** nothing in W2's output. [`../../mcp-readonly-pilot-contract.md`](../../mcp-readonly-pilot-contract.md) § W2 keeps its literal ASCII-`lower`/`instr` rule, ranking, limits and truncation. This record adds only how candidates are found.
- **Not decided here:** the search-side table and the rest of D1–D13, which stay with [#211](https://github.com/SquashMerge/trellis/issues/211). Whether one table could serve both stays open.
- **Compatibility:** none needed for disposable derived indexes. The new tables change the index format, so the index schema version changes and existing indexes are recreated, not migrated.

## Decision

1. **The table.** An FTS5 table with `tokenize='trigram case_sensitive 1'` holds an ASCII-`lower`ed copy of each declaration's name and ID. It is only a candidate prefilter: it never scores or orders. It may hold every retained document version; the answering revision is selected by the usual manifest join. The copy is written with SQLite's own `lower`, the same function the literal rule uses, so trigram folding can't diverge from it.
2. **The query.** A query is ASCII-`lower`ed the same way, then quoted as one FTS5 string literal with `"` doubled. FTS5 operators, column filters and parentheses in the query are therefore plain text.
3. **Verification.** Every candidate is still checked with W2's literal ASCII-`lower`/`instr` rule, and ranked and limited exactly as before.
4. **Literal-scan fallback, from the query alone, before FTS5 runs.** A query with fewer than three Unicode characters forms no trigram (`éa` is three bytes but two characters). A query containing U+0000 can't be carried by an FTS5 string literal. Both take W2's literal scan.
5. **Errors after FTS5 runs never fall back.** SQLite `BUSY`/`LOCKED` is the internal `storage_busy`, retried within the call's deadline, then W8's `deadline_exceeded`. Corruption (including a malformed FTS5 index), schema mismatch and I/O faults are `store_unavailable`.
6. **Output.** W2's output stays byte-identical for every valid 1–256-byte query: the same hits, order, truncation and limits. Its 256-byte query limit is unchanged.

## Implementation notes

- `native_declaration_trigrams` serves W2 over `native_version_declarations`. The browser `/api/symbols` and CLI `symbols` search runs the same literal rule over `graph_nodes`, so it gets its own table, `graph_node_trigrams`, under the same rules. Neither table is a search index for #211.
- Both are contentless (`content=''`, `columnsize=0`) and keyed by the source row's `rowid`. Triggers on the source table keep them in step on insert, delete and update, so every existing publication, retention and rebaseline path maintains them. A delete uses FTS5's `'delete'` command with the same `lower`ed values the insert wrote; `contentless_delete=1` was measured about 7× slower to insert at 10k.
- FTS5 phrase matching needs `detail=full`; `detail=none` and `detail=column` reject a multi-trigram string.
