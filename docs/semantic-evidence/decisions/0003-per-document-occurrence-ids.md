# Decision 0003: per-document occurrence identity, revision-scoped semantic validity

- **Status:** proposed amendment for owner ratification. It changes normative text in `../contract-v1.md` (#22), `0002-publication-rejoin.md` and `../publication-rejoin-vectors-v1.md` (#11A). It changes no deployed schema by itself; implementation follows in separate reviewed work (see [Implementation impact](#implementation-impact)).
- **Scope:** occurrence identity (`OccurrenceId`), the extraction-context digest it depends on, and the rules that relied on occurrence IDs being revision-bound. Syntax identity (`SyntaxId`), canonical bytes, `Revision.id`, coverage, freshness, warnings-v1, and the 11A raw envelope and proof rules are unchanged.
- **Compatibility: none.** Pre-release, the owner requires no backward compatibility. There is **one** occurrence identity, `occ:v2`. The revision-bound `occ:v1` derivation is withdrawn everywhere. This supersedes Decision 0002's clause keeping legacy #26 `formatVersion:1` example bytes immutable, as far as those bytes carry occurrence IDs: the frozen normalized `formatVersion:1` fixture records and the #57 v1 vector rows are regenerated under `occ:v2`.

## Problem

Under v1, an occurrence ID hashed `{revisionId, ownerSyntaxId, kind, ordinal}`, and `Revision.id` is a complete-snapshot identity covering every source document, the toolchain, config, dependency captures and the native extractor. Any edit anywhere therefore produced a new revision, and with it a new ID for **every** call, control region and reference in **every** document.

A store that materializes native evidence must rewrite O(workspace) rows on each publication, inside the writer transaction. #67 found that this cannot meet its bounded writer hold or its single-file update targets (medium p95 ≤2 s, large ≤5 s) at the ratified 50,000/500,000-fact floors. The alternative of deriving revision-bound IDs at read time keeps the cost permanently: per-read derivation, plus an index to resolve occurrence IDs that can't be inverted. It also complicates fact caching (#71), worktree reuse and incremental rejoin.

## What revision binding was protecting

The v1 rule "occurrence-keyed evidence never crosses revisions" protects **semantic validity**, not syntax. A document can be byte-identical across revisions while a fact *about* one of its occurrences becomes wrong: an unchanged call to `g()` whose target `g` changed in another document. Revision-bound occurrence IDs enforced this by making every binding expire at every revision. The same guarantee is now stated directly on semantic records, where the staleness actually lives.

## Decision

1. **Extraction context.** For each document, `extractionContext` is the full SHA-256 over domain `trellis.extraction-context.v1\0` and the #22 canonical bytes of `{language:Language, components:[{name:Text, hash:Hash}]}`.
   - `components` lists **every** non-source input that the native producer reads for that document's language and that can affect any native measured field or projection: configuration, toolchain and dependency captures, each by its captured component digest. They are sorted by `name`, then `hash`, and unique.
   - A producer that reads no such input has `components: []`.
   - The native producer declares which components it reads. Starting to read another input is a native producer version change (item 3).
   - This digest is the **extraction-context component** of #71's path-neutral cache key `(language, extractor version, extraction-context digest, content hash)`. #71 must not define a different context digest.
   - **Authentication, fail-closed.** Before an `extractionContext` (or an occurrence ID derived from it) is treated as identity or cache evidence, the producer's declared component inventory and every component digest must be verified against the authenticated revision capture and the native producer descriptor. **No `occ:v2` record may be minted or published** until the producer version's complete input inventory and every component capture are authenticated against the pinned revision. An undeclared, missing, stale or mismatched component, or an unproven `[]`, fails that precondition: the affected documents get no `occ:v2` records and publication fails closed for them. Re-measuring cannot repair it, because re-measurement doesn't establish what the producer actually reads. Retry only after the precondition succeeds. This is distinct from an ordinary cache miss: a missing or corrupt **optional** cache entry whose inputs are validly attested may simply trigger normal measurement. The v2 hash proves its inputs match, not that they were authentic.
   - **Strict document locality.** Native measurement of a document depends only on its own bytes and its extraction context. Under `occ:v2`, a native producer **must not** read any other source document to measure a document. Cross-document source dependencies are not representable in v2. A later identity version may add them, with full `DocumentKey` component identity (`sourceSetId`, `language`, `path`) verified against the pinned revision capture.
2. **Occurrence identity is per document version and context.** The occurrence input is exactly

   ```text
   {contentHash:Hash, extractionContext:Hash, nativeProducerId:Text, nativeProducerVersion:Text, ownerSyntaxId:SyntaxId, kind:call|reference|control, ordinal:UInt}
   ```

   - The domain is `trellis.occurrence.v2\0`, and the emitted form is `occ:v2:` followed by the first 32 lowercase hex characters (16 bytes) of the full SHA-256.
   - `contentHash` is the containing document's exact content digest.
   - `nativeProducerId`/`nativeProducerVersion` are the native producer descriptor's `id` and `version`.
   - `ownerSyntaxId` is the emitted 128-bit owner ID, which binds the source set, path and language.
   - Ordering, ordinal namespaces and duplicate rejection are unchanged. Collision handling for the two native ID namespaces is subsequently narrowed by [Decision 0015](0015-native-hash-based-identity.md): only a matching typed 16-byte handle with a different full digest is detected and rejected.
3. **Native producer versioning.** Any extractor change that can alter **any** native measured field or projection of any occurrence or declaration **must** change `nativeProducerVersion`. That covers owner, kind, ordinal, range, callee range, spelling, lookup key, region membership, parent and arm, and coverage. #71 relies on the same rule. The executable hash stays in `Producer.executableHash`, provenance and `Revision.id`, but is not occurrence identity.
4. **When IDs are equal.** Two revisions share an occurrence ID exactly when the document has identical bytes (`contentHash`) at the same `DocumentKey` (through the owner ID), under the same native producer ID and version and the same authenticated `extractionContext`, with the same owner, kind and ordinal. Under items 1 and 3, those inputs determine identical native measured output. What a change re-identifies:
   - a byte change to one document re-identifies all of **that** document's occurrences and no other document's;
   - a change to a shared context component re-identifies every document whose context includes that component;
   - a native producer ID or version change re-identifies every document that producer measures.
5. **Semantic validity stays revision-scoped.**
   - Every semantic record, binding and join keyed by an occurrence ID (`CallBinding`, `Reference` resolution and targets, `Join` with occurrence candidates) is valid **only at the revision of its provenance**. That is the captured revision for captured evidence, and the destination revision for publication-rejoined evidence.
   - A shared occurrence ID never carries a binding, resolution, target, join or provenance from one revision to another.
   - After a failed or omitted r2 refresh, r2 occurrences have no selected-producer binding even when their IDs equal r1's.
   - Freshness rule 2 (`possiblyStale`), `staleTarget=false` and the no-expansion rules are unchanged.
6. **Pinned projection (acceptance condition).** A pinned read of revision r2 projects every native record of a reused (unchanged) document version **as r2 evidence**:
   - its `.revisionId` is r2;
   - its native measured-syntax provenance and coverage are r2's;
   - no r1 provenance, reference resolution, target or join becomes r2-valid because an occurrence ID matches.

   The physical layout (item 9) is informative, but this projection is required of any implementation.
7. **11A rejoin.** For a byte-identical document at the same `DocumentKey`, under the same native producer ID, version and extraction context, the r2 native occurrence ID **equals** the r1 ID; otherwise it differs.
   - Rejoin still requires a **separately verified** r2 native candidate at the converted span and kind. It mints **new r2 provenance** with the verified `derivedFrom`, and r2-scoped associations with the r2 internal target revision.
   - All 11A checks are unchanged: identical authenticated bytes and key, exactly one distinct compatible r2 candidate, the same-`SyntaxId` target rule, the A-`failed` / B-`partial` dispositions, freshness and no-expansion.
   - ID equality is never a validity proof. No r1 provenance or binding is carried into r2.
8. **Cross-revision links.** Stable syntax IDs, and occurrence IDs under item 4's conditions, are the only cross-revision identity links. Neither carries semantic validity across revisions. The #47 historical-evidence scope is unchanged: no old call binding is applied.

## Storage consequence (informative)

9. Publication can write only the changed document versions' native rows plus a revision → document-version manifest. Pinned reads resolve through that revision's manifest and project item 6's r2 fields, and document versions that no retained revision references can be garbage-collected. This is the model #67's native-correctness work should implement. No read-time ID derivation or occurrence-ID resolution index is needed.

## Vectors

[`../publication-rejoin-vectors-v1.md`](../publication-rejoin-vectors-v1.md#occurrence-identity-v2-decision-0003) gives the exact `extractionContext` and `occ:v2` rows for the #57 fixture's A.js call and reference. They include controls for a changed native producer version and a changed extraction context, and they replace the withdrawn v1 rows. They were computed from #22 canonical bytes with `tools/semantic-contract/json.mjs::canonicalBytes` and independently re-canonicalized and hashed with Python `hashlib`. The same method reproduces the withdrawn v1 `r1/call` digest (`959c5606…`).

## Implementation impact

None of this is done by this docs change:

- **`src/native_ids.rs`:** occurrence registration switches to domain `trellis.occurrence.v2\0`, prefix `occ:v2:` and the new input. `src/native_evidence.rs`: compute and declare `extractionContext`, and bump the native producer version.
- **`src/mcp/catalog.rs`, `src/mcp/tools.rs`:** the `occ:v1:` ID patterns become `occ:v2:`.
- **`tools/semantic-contract/identity.mjs`** and every semantic-contract test and fixture that computes or pins occurrence IDs (`formats`, `identity`, `normalization`, `record-*`, `graph*`, `answers`, `counts`, `example-fixture`). That includes the frozen normalized `formatVersion:1` records (`tools/semantic-contract/schema.mjs` normalized record shapes), regenerated under `occ:v2` with their published hashes updated.
- **Native tests** (e.g. `tests/python_indexer.rs`) that pin occurrence IDs.
- **#71:** key on item 1's `extractionContext`.
- **#67:** the native-correctness slice is re-scoped to per-document storage, a revision manifest and item 6's projection. **#87–#90:** wording that assumes r2-minted occurrence IDs (#89 rejoin and #90 in-transaction rejoin) is updated to "same IDs for unchanged documents; new r2 provenance and associations".
