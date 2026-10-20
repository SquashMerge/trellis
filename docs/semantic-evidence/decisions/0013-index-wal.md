# Decision 0013: `index.db` uses WAL so prior-head reads survive a publication

- **Status:** **draft for review, not ratified.** The owner asked for this draft on 2026-10-19 after #210's measurement. Prime's design review of #210 set its scope. Nothing here changes behaviour until the owner ratifies it. Code follows ratification, not this draft.
- **Amends, on ratification:**
  - `../../local-topology.md` T03 (reads during publication) and T06 (recreation, sidecars, GC);
  - [Decision 0006](0006-revision-retention-and-index-gc.md) §2 (the "hot journal" GC condition);
  - [Decision 0008](0008-daemon-sole-writer.md) §5, its journal-mode statement, **for `index.db` only**.
- **Does not amend:** Decision 0008 §3 "Open the queue". It describes `requests.db`, which keeps its rollback journal.
- **Compatibility:** none. Indexes are disposable derived state; existing rollback-mode indexes are recreated as an obsolete format (§2). Nothing is migrated or converted.

## Background

`index.db` runs in `journal_mode=DELETE` with `synchronous=FULL`, and `open_index` refuses any other mode. A publication is one atomic transaction (T05).

#210 measured a full republication of the 10,000-file corpus, every document changed, on a shared machine (load ≈8–9). Evidence: `~/projects/odin/trellis-evidence/issue-210/`; Prime's measurement and design reviews are alongside it.

| | main `2d11efa` | with #209's 256 MiB writer cache |
|---|---|---|
| Republication | 688 s | 540 s |
| A zero-timeout read-only probe from another process could not read | one stretch of ≈399 s | one stretch of ≈300 s |
| Browser reads in that stretch (`/status`, `/files`, `/tree`, `/symbols`) | `409 storage_busy` | `409 storage_busy` |
| Rollback journal file length at peak | ≈2.08 GB | ≈2.07 GB |

**What was not tested:**
- **Read paths:** MCP, CLI and selected-source reads.
- **Exact timing:** the clocks start at the file edits, not at the transaction's BEGIN. A zero-timeout probe can't tell PENDING from EXCLUSIVE. The journal length is not a measurement of spilled cache bytes.

So a spill-driven EXCLUSIVE lock is the likely cause, not a proven sole cause. These are single runs.

Design reasoning, not a measured impossibility: one atomic publication can be arbitrarily large, and no finite cache can guarantee that its dirty pages never spill. In a rollback-journal database, a spill blocks every reader until COMMIT. In WAL mode, readers keep the last committed snapshot while a writer runs, without depending on the writer's cache.

## Decision

### 1. Scope

- **`index.db`:** uses `journal_mode=WAL`, with `synchronous=FULL` on **every writable connection**.
- **Unchanged:**
  - `requests.db` and durable `workspace.db` keep `journal_mode=DELETE` and `synchronous=FULL`;
  - one atomic transaction per publication;
  - one writer (Decision 0008);
  - validated-head admission;
  - selected-document attestation;
  - generation/revision CAS;
  - `data_version` fences.
- **Goal:** prior-head reads keep being served while a publication is in progress. Startup reconciliation and the rules for reading during it (T03) don't change.

### 2. Existing indexes are recreated, not converted (owner ruling, 2026-10-19)

Trellis is pre-release with no backward compatibility, and indexes are disposable. A rollback-mode `index.db` is therefore an **obsolete index format**, handled like any other disposable format change:
- **Detection:** the implementation bumps the index schema version, so the existing obsolete-format marker check recognizes a rollback-mode file.
- **Rebuild:** the existing exceptional-recreation path (T06) rebuilds it as a WAL index under verified exclusive, non-blocking index use. If exclusive use isn't available, recreation defers with typed `storage_busy`.
- **What survives and what doesn't:**
  - `requests.db`, durable records, UUID markers and facts survive;
  - the new index gets a new generation, so old pins conflict, as with any recreation.
- **Failures:** ordinary busy, I/O and full-disk failures never authorize the deletion itself (T06). Only the obsolete-format classification does.
- **No conversion code:** there is no in-place conversion. `open_index` admits only `wal` for `index.db`, and new indexes and recreation stages are created in WAL.

### 3. A WAL file's presence is not evidence of a crash

- **Never a classification:** `index.db-wal` and `index.db-shm` exist whenever the database is in use or hasn't been checkpointed. Their presence alone never classifies the index as crashed, corrupt or in need of recovery.
- **Recovery is SQLite's:** SQLite's own WAL recovery runs on the first open after a crash. It applies committed frames and ignores incomplete ones. That open happens in the daemon, under the single-instance lock and shared checkout use, **before** header and schema validation. Validation then judges the recovered file, as today.
- **Never unlinked:** Trellis never manually unlinks an uncheckpointed `-wal` or a `-shm`.
- **Recovery outcomes stay typed:**
  - `SQLITE_BUSY_RECOVERY` and `SQLITE_BUSY_SNAPSHOT` are `storage_busy`, with bounded retry;
  - `SQLITE_READONLY_RECOVERY`, `SQLITE_READONLY_CANTINIT` and `SQLITE_READONLY_DBMOVED` are typed failures, never generic errors;
  - corruption takes the existing corruption path. Only that path may lead to exceptional recreation.
- **`reject_sidecars`** accepts `-wal`/`-shm` for `index.db`. A `-journal` beside a WAL index is unexpected and stays `recovery_required`; one beside an obsolete rollback-mode index is handled by §2's recreation.
- **No separate file handles:** the rule against closing a separate descriptor on a live SQLite inode applies to the WAL and shm files too. Size checks use path metadata, never an opened descriptor.

### 4. Existing-only and report opens

Some paths must not write the index, among them `trellis gc --report` and the orphan-queue inspection through `readonly_index_db`. In WAL mode, a read-only SQLite open can **create** `-wal` and `-shm` (measured in `../../mcp-pilot-slice0-findings.md` §2).

- **Split by database:** `readonly_index_db` is split into separate index and queue variants. The queue keeps today's rollback-mode checks.
- **The index variant writes nothing:**
  - if `-wal` or `-shm` exists, the entry is reported `busy` without opening SQLite;
  - otherwise contents are read only under a verified non-blocking **exclusive** index use lock, which proves no live opener, opened `immutable` and read-only, then released;
  - if exclusive use isn't available, the entry is reported `busy`.
- **Daemon reads are unaffected:** daemon-routed reads run in the daemon, whose 0700 state directory can hold the sidecars.

### 5. Checkpoints

- **When:** automatic checkpoints are off on the publication connection (`wal_autocheckpoint=0`). The checkout worker owns checkpointing as a maintenance unit, at the lowest priority (Decision 0009), after each COMMIT:
  1. first a `PASSIVE` checkpoint;
  2. then a `TRUNCATE` checkpoint, attempted with a bounded busy wait only when no reader holds an older snapshot.

  Each result's `(busy, log frames, checkpointed frames)` is inspected and recorded.
- **Failure is maintenance failure only:** a checkpoint failure after COMMIT never rolls back, fails or changes a committed request. Terminal request outcomes stay monotone, and the accepted row stays durable. The failure is reported in status and retried.
- **WAL budget and backpressure:**
  - one publication can write ≈2 GB of WAL at 10k files;
  - a reader holding an old snapshot prevents the WAL from being reset;
  - Trellis therefore can't promise both a strict WAL size ceiling and unconditional progress for new writes alongside an arbitrarily long reader.
  - **The draft chooses bounded WAL with deferral:** before starting a publication, if the WAL exceeds the budget, the worker tries a bounded `TRUNCATE` first. If the WAL is still over budget, the publication defers with typed `storage_busy` and retries. The request stays queued, durable and in FIFO order.
  - Every Trellis read has a deadline (#142; MCP 5 s), so only a stuck handle can hold a snapshot indefinitely, and that is a defect to report.
  - *The budget value is an owner decision.* The draft proposes 4 GiB, about twice the largest measured publication.

### 6. Staged replacement and GC

- **Exceptional recreation (T06):** the stage is built in WAL, `TRUNCATE`-checkpointed, and has every connection closed. It is verified to have no `-wal` frames left (SQLite removes the files on last close) before the rename over `index.db`.
  - The live index's connections are closed and its WAL checkpointed under the exclusive use T06 already requires, before the rename. This prevents a stale `-wal` or `-shm` from attaching to the new file.
  - A failed checkpoint, close or rename leaves the old index in place and reports failure. It never reports partial success.
- **GC (Decision 0006 §2):** the condition "skip anything … with a hot journal" becomes "skip anything with a `-journal`, or with a `-wal` that the deleting worker can't checkpoint to empty under verified exclusive use". Under exclusive use the worker opens writable, `TRUNCATE`-checkpoints and closes. Only then does it delete `index.db`, `-wal` and `-shm` together. Any failure skips the entry.

### 7. Snapshots

- **Per read:** each read keeps one SQLite read transaction for its snapshot, as today.
- **Fences:** `data_version` fences stay on the same connection.
- **Connections don't share snapshots:** two connections reading the same pin don't automatically see the same snapshot. Any design that reads one revision on several connections (#199 Q-6) must establish that itself.
- **Unchanged:** selected-document attestation.

## Required tests (hook-synchronized, per CLAUDE.md)

1. **Reads during a real publication:** a validated prior pin stays readable while it runs, through the browser, selected source and an MCP read. A fresh new head is read after COMMIT.
2. **Kill before COMMIT** (incomplete WAL frames): reopen yields the old committed pin, and queue FIFO reconciles with no false ACK and no terminal state moving backwards.
3. **Kill after a durable COMMIT, before checkpoint and before queue completion:** reopen yields the new committed pin, and the request completes once with the right outcome.
4. **Old reader held through commits:** hold an old-snapshot reader through several commits and a partial checkpoint. Release it, and the `TRUNCATE` completes.
5. **Budget backpressure:** with the WAL over budget and a held reader, the next publication defers with typed `storage_busy` and completes after release.
6. **Injected faults:** busy, full-disk, I/O and permission faults injected into recovery and checkpoints all stay typed. A checkpoint fault never fails a committed request.
7. **Obsolete format (§2):** a rollback-mode index is classified obsolete and recreated as WAL, keeping `requests.db`. A busy recreation defers without deleting.
8. **Stage replacement:** rename and fsync failures, and a stale WAL, never attach to the new file.
9. **GC:** skips a live or uncheckpointable WAL, and deletes all three files together.
10. **Report path (§4):** never creates sidecars, and reports `busy` when it can't prove there's no live opener.

Existing tests specific to the rollback journal, such as the `daemon_crash_recovery.rs` hot-journal kills, are replaced by 2 and 3 for `index.db`, and are kept as they are for `requests.db`.

## Open owner decisions

1. Adopt WAL for `index.db` as scoped here.
2. The WAL budget value and the deferral policy (§5).

## Consequences

- **Reads stay available:** prior-head reads are served throughout a publication of any size, subject to the tests above.
- **Disk use:** grows by up to the WAL budget between checkpoints.
- **New worker unit:** checkpoint maintenance, with status reporting.
- **More states to handle:** the recovery, sidecar, stage and GC code gains WAL-aware states. This is some of the most closely reviewed code in the repository, so it's medium-to-large work, scheduled after #199's in-flight items.
