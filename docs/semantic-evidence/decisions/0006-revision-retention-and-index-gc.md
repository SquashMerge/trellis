# Decision 0006: revision retention, automatic index deletion and unchanged requests

- **Status:** owner-approved 2026-10-05: the 15-minute supersession grace with no reader-renewed lease, guarded whole-index deletion invalidating that generation's pins, and the unchanged path for explicit requests. Ratified when this record merges. §3 amended 2026-10-20 (#199 R-3/S-4, [below](#amendment-2026-10-20-no-revision-for-an-unchanged-capture)). It adds revision retention to `../../local-topology.md`, confirms that document's existing automatic GC rule, and governs #16.
- **Scope:** which published revisions stay readable, when a whole derived index may be deleted automatically, and how an explicit indexing request with no source change publishes. Pin identity (`{indexGeneration, indexRevision}`), #67's storage model and Decision 0005 are unchanged.
- **Compatibility:** none needed (pre-release).

## Background

#67 retains **every** revision of the active generation and ships the release and GC mechanism (`release_revision`, `collect_unreferenced`), but no policy. Each revision carries a full manifest, one row per document. With #16's watcher publishing on every save, retaining everything grows without bound. Storage then grows with the number of publications times the number of documents, regardless of whether anyone still reads the old revisions.

A derived index is a disposable cache: deleting it only costs a rebuild. An **old revision**, however, can't be rebuilt, because a rebuild indexes the source as it is now. Retention therefore exists only so that a reader working at a pin isn't interrupted mid-task by the next publication. It is not a history feature.

## Decision

### 1. Revision retention: head plus a short supersession grace

- **What's retained:** the **current head**, plus every revision that stopped being head **less than 15 minutes ago**. A revision's supersession time is the publication time of the revision that replaced it.
- **Release:** once a non-head revision is past the 15-minute grace, the leader releases it and collects unreferenced document versions and projections. GC always runs **outside** the publication transaction, and never removes anything a retained revision references.
- **Expired pin:** a pinned read or write at a released revision fails with a typed conflict (pin expired or released). It **never** silently moves to a newer revision. The client re-reads at head.
- **Readers write nothing.** Retention needs no lease renewal or other reader-side writes, so the T06 rule that followers never write index or request state to read still holds.
- **Saved views and notes retired** by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). Their files remain in place and ignored; no loading or reattachment path remains.
- **The grace is a policy constant.** Changing it later is an owner decision, not a format change.

Durable per-reader pin leases were considered and rejected for now. Renewing a lease on every read would make readers write shared state, against T06. If longer-lived pins are needed later, a separate decision must define where leases are stored and why readers may write them.

### 2. Automatic deletion of whole derived indexes

- **Eligible:** a derived index directory whose recorded root is **missing or replaced**, or which has **not been opened for 30 days**. This is the existing `local-topology.md` rule, unchanged; this decision confirms it and adds the conditions below.
- **Earlier schemas (owner ruling 2026-10-23, #253):** an index whose schema version predates the current one is eligible whatever its layout, root or age. Its directory goes with it, including `requests.db` and any accepted rows in it.
- **Conditions, all required:**
  - for a current-schema index, fresh checks of the exact schema, root identity and recorded age; for an earlier schema, a fresh read of its schema version;
  - a verified **exclusive, non-blocking** use lock;
  - skip anything live, busy, with a hot journal, unknown or unreadable.
- **Never deleted automatically:** durable records, UUID markers, configured token and ledger trees, and legacy state.
- **Pins:** deleting an eligible index invalidates every pin of its generation, including revisions that would otherwise be retained. The retention rule in §1 protects revisions **within a live index**; it doesn't keep an eligible index from being deleted. Reopening the workspace creates a new generation, and old pins conflict.
- **Disk space:** the deleting process must not keep descriptors open on the deleted files, or the space isn't reclaimed. #16 must reconcile this with #67's retained SQLite check-handle cache.
- `gc --report` stays read-only. Its "eligible" never authorizes deletion by itself.

### 3. Explicit requests with no source change

- After its normal FIFO claim, an explicit CLI or browser indexing request whose capture shows no change uses the **guarded unchanged path**, as startup reconciliation does since #67.
  - It freshly hashes every admitted source.
  - It compares the fresh source hashes and lengths with the indexer's recorded source metadata, and checks the same generation, producer binding and executable, extraction context, options, capture inputs and projection links, with the CAS and `data_version` fences. It trusts the source BLOBs validated at publication; it does not read, hash, UTF-8-decode or recheck their encoded budget on this path (#261; clarification applying #261 for #300, 2026-10-25).
  - It publishes **nothing**: the request is acknowledged `done` with the current head's pin (amended 2026-10-20; it used to publish a new revision header and manifest copied from the head).
- A source change, or a changed option or input, takes #67's local or full path instead. Executable drift requires **full fresh measurement** (Decision 0005), never the local path.

## Consequences

- **Storage:** Storage is bounded by the head plus publications in the last 15 minutes; #73’s one-retained-revision steady gate is a separate measurement boundary.
- **Long agent sessions:** a session that holds a pin for more than 15 minutes after the next publication gets a typed conflict and must re-read at head. Agents and MCP clients treat that as normal.
- **Unused workspaces** lose their index after 30 days unopened and pay a cold rebuild the next time they're opened (about 70 s medium, about 11 min large, extrapolated).
- **No-op index requests** are expected to become fast, about 0.35 s on medium instead of a full re-measure. That's an expectation based on #67's measured unchanged leader `serve` start, not a measured result for explicit requests.

## Implementation impact (#16)

- Record supersession time (for example from the next revision's publication time), and release revisions past the grace. Schedule `release_revision` and `collect_unreferenced` outside publication: after publications and periodically.
- Add a typed expired-pin conflict, and test that it never moves to a newer revision.
- Implement guarded automatic deletion per §2, using the existing 30-day `GC_AGE_SECONDS` (`src/store/topology.rs`).
- Route unchanged explicit requests through the guarded unchanged path after their FIFO claim.
- Release retained SQLite check handles for deleted or recreated files (see #16's scope).

## Amendment (2026-11-01): head-rooted release holds

Owner ruling on #573, 2026-11-01. It amends §1.

**What holds a revision past the grace.** Two exceptions to the 15-minute release, both rooted at the head:
- the captured revision of a batch the head selects; a rejoin rebuilds from its native header, manifest and version links;
- the revision that took the import of a batch the head selects, the earliest revision selecting it; its admitted tuples are what a witnessed rejoin reuses (Decision 0002).

A selection row on any other revision holds nothing, so sequential imports leave no chain. The holds are whole revisions, not extracted witness data, and no thinning of a held revision is done.

**Old pins.** A superseded revision keeps its full 15-minute grace, and a read pinned to it still works if its captured revision is released first: it reads its own stored basis and raw proofs, and raw cleanup stays scoped to every remaining selection. Past the grace, a pin to a revision no hold keeps stops resolving, as in §1. A held revision is a dependency of the head, not a promise of history, and fewer revisions are available to historical declaration lookups.

**Storage.** With P producer slots, after the grace and once release has drained: the head plus at most 2P full revisions, plus revisions still in their grace and unfinished release debt. This is independent of the number of imports, but it isn't literally zero growth and isn't a deadline:
- Held revisions keep their own incidental selections, so more raw batches survive than the head selects: two for one producer, and possibly O(P²).
- A released revision keeps its `native_revisions` header row as a tombstone.
- Freed SQLite pages need not shrink the file.
- New producer identities raise P.

The "Storage" and §1 statements above that retained revisions are the head plus the last 15 minutes are read with these holds.

## Amendment (2026-10-20): no revision for an unchanged capture

Approved by the owner on #199 (items R-3 and S-4). Decision 0009 §2 is amended with it.

**Why.** The unchanged path copied the head's header, manifest, capture inputs and producer binding into a new revision. The copy carried no new information, but at 10k documents each one added 17–34 MB, and every unchanged daemon restart or no-op request added one.

**What counts as unchanged.** A capture is unchanged when all of these hold, decided only from values Trellis already computes for every capture and records with every revision:
1. **Same head.** The selected head is the request's CAS baseline: the same `{indexGeneration, indexRevision}` (revision > 0), a compatible validated baseline with the same witness, and no recovery pending.
2. **Same producer.** The head has a producer binding, and its bound producer hash equals the hash of the executing binary taken at this capture's admission (Decision 0005).
3. **Same options.** The head's recorded reconcile options equal the capture's.
4. **Same capture.** The capture's freshly read and hashed sources (path, content hash and capture stat) and its persisted capture inputs (root, directories, config, selector and presentation observations) equal the head's manifest and recorded capture inputs exactly: nothing added, deleted, stat-changed or hash-changed, and at least one entry examined.

A copy made under these conditions would equal the head in everything but its revision number and `indexed_at`. Anything else takes the local or full path and publishes, as before. The decision does not re-validate stored rows. The path trusts source BLOBs validated by the indexer and compares the fresh capture's hashes with their recorded metadata. Projection links, extraction context, class catalog budget and publication fences remain fail-closed; a guard failure still fails the request without acknowledging it.

**Clarification applying #261 (#300, 2026-10-25).** Fresh working-tree capture hashing remains required. Pure no-write UNCHANGED completion uses that already verified capture and the matching pinned metadata without a second full capture verification. A rebuilt-executable metadata publication retains its publication verification. The existing `covers_watcher_request` certificate keeps its request-to-watcher acknowledgement direction: its root/options identity check need not repeat verification already performed by capture admission or successful publication. It does not become reverse watcher-to-request authority. A just-published watcher head can satisfy a later request through the same fresh-hash/pinned-metadata comparison. FIFO, H, watcher ordering, atomic publication and durable terminal completion are unchanged.

**What happens instead of publishing.**
- No write transaction runs. The path's fences still hold: the index's `data_version` is unchanged since admission, the writer guard and workspace identity verify, and a re-read of the head pin and baseline witness still matches.
- An explicit request completes as terminal `done` with the current head's pin, and its waiters wake through the terminal-commit counter as for any completion.
- Startup reconciliation attests the unchanged head for the new writer incarnation and enters `Ready` (Decision 0009 §2), so `catchingUp` clears and FIFO claims proceed in their unchanged order.
- **Pins and retention.** The head stays head and is not superseded, so it gets no supersession stamp and starts no grace period. A pin on it stays valid and is still the current head, so a CAS on it succeeds. Older retained revisions keep their original supersession times. `indexed_at` keeps the time the head was published.
- The watcher's final-inventory check (`selected_capture_unchanged`) already published nothing for an unchanged capture; its gate ordering is unchanged.

