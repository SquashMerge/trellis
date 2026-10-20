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
- **Saved views and notes.** Loading one at an expired pin **conflicts**, like any other pinned read. It never falls forward silently. The caller may then explicitly request head and reattach the item there through its durable anchors (#65). #97 shows when a result comes from an older revision than head.
- **The grace is a policy constant.** Changing it later is an owner decision, not a format change.

Durable per-reader pin leases were considered and rejected for now. Renewing a lease on every read would make readers write shared state, against T06. If longer-lived pins are needed later, a separate decision must define where leases are stored and why readers may write them.

### 2. Automatic deletion of whole derived indexes

- **Eligible:** a derived index directory whose recorded root is **missing or replaced**, or which has **not been opened for 30 days**. This is the existing `local-topology.md` rule, unchanged; this decision confirms it and adds the conditions below.
- **Conditions, all required:**
  - fresh checks of the exact schema, root identity and recorded age;
  - a verified **exclusive, non-blocking** use lock;
  - skip anything live, busy, with a hot journal, unknown or unreadable.
- **Never deleted automatically:** durable records, UUID markers, configured token and ledger trees, and legacy state.
- **Pins:** deleting an eligible index invalidates every pin of its generation, including revisions that would otherwise be retained. The retention rule in §1 protects revisions **within a live index**; it doesn't keep an eligible index from being deleted. Reopening the workspace creates a new generation, and old pins conflict.
- **Disk space:** the deleting process must not keep descriptors open on the deleted files, or the space isn't reclaimed. #16 must reconcile this with #67's retained SQLite check-handle cache.
- `gc --report` stays read-only. Its "eligible" never authorizes deletion by itself.

### 3. Explicit requests with no source change

- After its normal FIFO claim, an explicit CLI or browser indexing request whose capture shows no change uses the **guarded unchanged path**, as startup reconciliation does since #67.
  - It freshly hashes every admitted source.
  - It checks the same generation, the stored source bytes, the producer binding and executable, the extraction context, options, capture inputs and the projection links, with the CAS and `data_version` fences.
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

## Amendment (2026-10-20): no revision for an unchanged capture

Approved by the owner on #199 (items R-3 and S-4). Decision 0009 §2 is amended with it.

**Why.** The unchanged path copied the head's header, manifest, capture inputs and producer binding into a new revision. The copy carried no new information, but at 10k documents each one added 17–34 MB, and every unchanged daemon restart or no-op request added one.

**What counts as unchanged.** A capture is unchanged when all of these hold, decided only from values Trellis already computes for every capture and records with every revision:
1. **Same head.** The selected head is the request's CAS baseline: the same `{indexGeneration, indexRevision}` (revision > 0), a compatible validated baseline with the same witness, and no recovery pending.
2. **Same producer.** The head has a producer binding, and its bound producer hash equals the hash of the executing binary taken at this capture's admission (Decision 0005).
3. **Same options.** The head's recorded reconcile options equal the capture's.
4. **Same capture.** The capture's freshly read and hashed sources (path, content hash and capture stat) and its persisted capture inputs (root, directories, config, selector and presentation observations) equal the head's manifest and recorded capture inputs exactly: nothing added, deleted, stat-changed or hash-changed, and at least one entry examined.

A copy made under these conditions would equal the head in everything but its revision number and `indexed_at`. Anything else takes the local or full path and publishes, as before. The decision does not re-validate stored rows. The path's existing fail-closed guards (stored source bytes, projection links and extraction context, class catalog budget) still run unchanged, and a guard failure still fails the request without acknowledging it.

**What happens instead of publishing.**
- No write transaction runs. The path's fences still hold: the index's `data_version` is unchanged since admission, the writer guard and workspace identity verify, and a re-read of the head pin and baseline witness still matches.
- An explicit request completes as terminal `done` with the current head's pin, and its waiters wake through the terminal-commit counter as for any completion.
- Startup reconciliation attests the unchanged head for the new writer incarnation and enters `Ready` (Decision 0009 §2), so `catchingUp` clears and FIFO claims proceed in their unchanged order.
- **Pins and retention.** The head stays head and is not superseded, so it gets no supersession stamp and starts no grace period. A pin on it stays valid and is still the current head, so a CAS on it succeeds. Older retained revisions keep their original supersession times. `indexed_at` keeps the time the head was published.
- The watcher's final-inventory check (`selected_capture_unchanged`) already published nothing for an unchanged capture; its gate ordering is unchanged.

