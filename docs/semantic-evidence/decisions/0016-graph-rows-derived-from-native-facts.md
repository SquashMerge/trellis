# Decision 0016: graph rows derived from native facts

- **Status:** owner-approved ruling 7 for [#199](https://github.com/SquashMerge/trellis/issues/199) (2026-10-19 rulings), implemented by [#241](https://github.com/SquashMerge/trellis/issues/241).
- **Scope:** disposable graph rows in the index; no wire, identifier, graph-hash or class-catalog change. Older index schemas are recreated, not migrated.

## Decision

For each selected document, one shared projection function derives the `Symbol`, `CallSite` and `ControlRegion` graph DTOs from the authenticated source bytes and native declaration, call and region facts. The writer uses that function to build and hash the graph. The reader uses the same function on the stored native facts. The stored `graph_hash` remains unchanged. Graph IDs, the stored per-file extraction result `F`, class hashes and serialized responses remain unchanged. *(Amended 2026-10-23, #261: selected reads no longer recompute `graph_hash` or compare the narrow node and label rows with the derivation. They trust the published index; a stored row that does not decode is corruption and schedules a re-index.)*

`graph_calls` and `graph_regions` are removed, as is `graph_nodes.payload`. The narrow `graph_nodes` table stores `(projection_id, id, name, kind)` for search, counts and class foreign keys. Its trigram triggers and Decision [0013](0013-symbol-search-trigram-prefilter.md)'s second name table remain. Revision documents supply paths. SCIP presentation labels are the graph input unavailable in native facts or source, so `graph_display_labels` stores `(projection_id, id, label)` only when a node has a label. Labels feed the derivation; names serve search.

The graph projection header and its `class_extraction_payload` (`F`) remain stored. `class_projections`, `classes` and `class_relations` also remain stored: class catalog composition depends on whole-revision input and bounded catalog caps, so deriving those rows on each read would cost more. Decision [0004](0004-compositional-class-catalog.md)'s per-file `F`, per-revision composition and class tests therefore need no amendment. Decision [0005](0005-same-generation-producer-drift.md)'s R-1 publication hash-based reuse remains intact, so it also needs no amendment. Selected-document read attestation was later removed by owner ruling (#261).

Canonical graph payload golden vectors must be captured from the old main writer, before changing its projection function. Compare every serialized derived DTO byte for byte with those independent stored payloads; check graph hash and class outputs separately. Output sorting is per reader and retains the existing limits and tie-breaks. Existing graph row-byte budgets remain equivalent to the old stored-column-plus-payload accounting.
