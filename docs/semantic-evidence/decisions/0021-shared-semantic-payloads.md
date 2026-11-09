# Decision 0021: shared semantic payloads, revision-scoped validity

- **Status:** accepted design (owner, 2026-11-08); ratified on merge. The owner accepted every recommendation of the [#680](https://github.com/SquashMerge/trellis/issues/680) design note, D1–D8, on 2026-11-08. Implementation follows in separate reviewed work (see [Implementation impact](#implementation-impact)).
- **Amends:**
  - [Decision 0003](0003-per-document-occurrence-ids.md) item 5: storage may share a validated semantic payload across revisions; validity stays revision-scoped (item 1 below).
  - [contract v1](../contract-v1.md): the "never crosses revisions" paragraph of [Basis and freshness](../contract-v1.md#basis-and-freshness), the #669 copy rule in [Native-first publication](../contract-v1.md#native-first-publication-and-older-basis-reads-620), and the new [Shared semantic payloads (#680)](../contract-v1.md#shared-semantic-payloads-680) section.
  - [Local topology](../../local-topology.md): T06 [shared semantic payloads](../../local-topology.md#shared-semantic-payloads-680) (storage, schema, GC).
  - Decision 0006, the MCP read-only pilot contract, `toolVersion`, wire shapes, the publication-rejoin vectors and corpora are unchanged (item 2).
- **Compatibility:** none (pre-release). Index schema 42; older indexes are recreated, never migrated (item 8). Retained producer inputs survive the rebuild (#632) and are re-imported automatically. Durable state is untouched.

## Problem

Under the #669 copy rule, every semantic catch-up S writes every unchanged document's validated tuple into the new revision again, re-keyed to the destination. A one-line edit therefore rewrites the whole semantic corpus, about 3.0 M rows for 651 tuples on the Trellis checkout, and each revision that retention holds keeps its own copy: 2,371 coverage rows served 683 distinct `(producer, document version)` pairs on the live index, a 3.5× duplication (#680 note, §1). Native evidence has had the opposite model since Decision 0003 item 9: content-addressed document versions and a per-revision manifest.

The #620 payload experiment keyed reuse on the settled output, so every document still paid for witness, rejoin and validation before reuse could be decided, and it bound the native revision into the key, which ruled out sharing across native edits. This decision keys reuse on inputs known before any work and stores rows revision-neutrally.

## Decision

1. **Shared storage, revision-scoped validity (D1).** Storage may hold one validated semantic payload, the settled facts of one `(producer, document)` tuple, and let several revisions name it.
   - Each revision keeps its own coverage manifest row per `(revision, producer, language, path)`, with `requested`, `selected` and `state`. Only that row makes a payload part of a revision.
   - A payload is written once, after the same validation as today, and never updated.
   - **Validity is not shared.** Decision 0003 item 5 still holds for every read: a semantic record, binding or join is valid only at the revision whose manifest names it, projected as that revision's evidence. A payload's presence under r1 never makes it valid at r2; only r2's own manifest row does, and that row is written only by a publication that has established reuse is correct for r2 (item 4).
   - The revision-relative fields, which the #669 copy re-keys today, are not stored. Every read at revision r projects them through one shared projection function, as Decision 0016 does for graph rows: the native revision named by coverage, references, join anchors and internal targets is read as r's native revision; rejoined provenance IDs follow item 2; freshness follows item 3. This set is exactly the set the copy re-keys today, no more.
   - This is the semantic analogue of Decision 0003 item 6 for native records: a pinned read of r2 projects a reused payload **as r2 evidence**.
2. **Rejoined provenance IDs are derived at read time (D2).** A payload stores a rejoined provenance's raw `derivedFrom` ID and kind; a read at r computes the destination ID with the existing derivation from the raw ID, r's native revision and the kind. The wire bytes equal today's copy output, so the publication-rejoin vectors, corpora, MCP smoke and golden payloads are unchanged.
3. **Freshness is per revision, stored at publication (D3).** Freshness is stored per `(revision, tuple, basis)` beside the manifest row, written by the publication that names the payload, under the settlement's existing rule. It is not derived at read, so the #650 re-admission `fresh` exception, which depends on the publication path, is kept.
4. **Reuse by input key, within the selected batch (D4).** A publication reuses a payload only when its input key matches: the semantic producer identity (ID, version, executable hash), the selected raw batch (descriptor digest and captured revision), the document's version and the versions of its recorded projection dependencies. Equal keys mean an equal settled tuple. Reuse is limited to the selected raw batch. Cross-batch reuse, where a fresh producer run reuses documents whose raw claims and resolutions are unchanged, is deferred to a separate owner decision after measurement.
5. **Restored versions are reused (D5).** A document restored to an earlier version whose payload a retained revision still names is a hit, like any other equal key.
6. **Omitted associations are never reused (D6).** A payload that omitted associations for an absent target is not reused, as the #669 copy does not copy it today. It takes the validated path.
7. **The time target is reported, not gated (D7).** The design targets are T1, a median S of 3 s or less for body and comment edits; T2, an S writes only the changed and dependent tuples' rows plus one manifest row per document; T3, with 3 or more retained revisions, semantic tuple bytes are at most 1.3× one revision's. Under the perf ruling a missed T1 is reported, not blocking. Acceptance blocks only if S is slower than the baseline below or T2 fails. The FULL `Cargo.toml` catch-up is a non-goal.
8. **Schema 42, recreated (D8).** The schema change is disposable derived state. Older indexes are recreated, never migrated (#253 GC old-schema ruling).

A payload that no retained manifest row names is collected like an unreferenced document version (local topology T06).

The #669 rule that a completed S equals a from-scratch FULL is unchanged and remains the oracle for wrong reuse.

## Baseline measurement

Slice 1 of #680 confirms the note's baseline on main `a3a729e9` with the 2026-11-05 dogfood method, on a scratch copy and an owned daemon, never the live daemon. The method, raw outputs and numbers are in the owner's evidence archive at `trellis-evidence/680-660-design/measure/RESULTS.md`; the PR that ratifies this decision quotes them.

## Consequences

- Rows written per S become O(changed and dependent payload rows plus documents), and retained revisions share payloads, so disk use approaches one revision plus the changed documents. Decision 0006's policy and the "head plus at most 2P full revisions" bound are unchanged.
- A corrupt shared payload affects every revision that names it. Reading it is the read path's typed integrity failure and schedules a re-index (#261), as today.
- An incomplete key is a correctness defect, not a T00 attacker. It is guarded by the S-equals-FULL oracle, property tests over edit, revert, dependency-edit and target-reappearance sequences, and item 6.
- The extractor hash and native producer version are unchanged: no native re-extraction or occurrence-ID change follows.

## Implementation impact

None of this is done by this docs change:

- `src/store.rs`: schema 42, `semantic_payloads`, fact tables keyed by payload, coverage manifest rows naming a payload, per-basis freshness rows, payload GC.
- `src/store/native_first/copy.rs` and `src/store/native_first.rs`: S reuse by key in place of the row copy; a same-native publication copies manifest rows only.
- `src/semantic_import.rs`: rejoined provenance IDs derived at read; projection dependencies record each dependency's version.
- Readers: fact joins move from `tuple_id` to the payload, through the shared projection function.
