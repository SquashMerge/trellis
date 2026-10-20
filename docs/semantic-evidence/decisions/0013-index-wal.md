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
- **Goal:** a publication no longer blocks prior-head reads. WAL removes the writer's lock as a reason a read fails. The read is still served only under the existing conditions:
  - a compatible, validated retained head exists;
  - the root is stable (T03);
  - storage is healthy;
  - the read meets its deadline and selected-document attestation.

  WAL bypasses none of these.
- **One effective target:** this decision and its amendments to T03, T06, Decision 0006 §2 and Decision 0008 §5 take effect together with their implementation. Until then the current `DELETE`-mode contract and code remain authoritative; ratifying this text doesn't make today's code non-compliant.

### 2. Existing indexes are recreated, not converted (owner ruling, 2026-10-19)

Trellis is pre-release with no backward compatibility, and indexes are disposable. A rollback-mode `index.db` is therefore an **obsolete index format**, recreated, never converted or migrated.

- **Normal opens are WAL-only:** `open_index` admits only `journal_mode=wal` for `index.db`. Its mode guard runs before `index_metadata` is read, so it can't itself recognize an old file as obsolete.
- **Separate obsolete-format probe:** classification uses a narrow, **non-serving** probe in the checkout worker. It runs before startup reconciliation, under the daemon single-instance lock and verified **exclusive, non-blocking** index use, with no other protected handle open:
  1. **Leftover journal:** if a `-journal` exists, a writable SQLite open first lets SQLite roll it back, as today's rollback-mode recovery does. No classification is made before that.
  2. **Read the format:** read the header's journal mode and the index format marker.
  3. **Rollback mode** with a well-formed marker for an older schema is classified **obsolete**. A malformed or unknown mode or marker is classified **corrupt** (T06's existing rules). Neither the probe nor anything it reads is ever served.
  4. **Recheck:** immediately before replacement, recheck that the same inode, mode and marker are at the pathname. If anything changed, abandon the replacement and probe again.
- **If exclusive use isn't available:** the probe defers with typed `storage_busy` and retries. It never deletes.
- **Recreation** uses T06's exceptional-recreation path (§6), building a WAL index:
  - `requests.db`, durable records, UUID markers and facts survive;
  - the new generation makes old pins conflict, as with any recreation;
  - ordinary busy, I/O and full-disk failures never authorize the deletion; only the obsolete or corrupt classification does.

### 3. WAL recovery and typed outcomes

- **Presence isn't a classification:** `index.db-wal` and `index.db-shm` exist whenever the database is in use or hasn't been checkpointed. Their presence alone never classifies the index as crashed, corrupt or needing recovery.
- **Recovery is SQLite's:** SQLite's own WAL recovery runs on the first writable open after a crash, applying committed frames and ignoring incomplete ones.
  - The checkout worker's writable open at activation performs it, under the single-instance lock and checkout use, **before** header and schema validation. Validation then judges the recovered file.
  - Read-only connections never perform recovery.
- **Unlinking:** Trellis never unlinks an uncheckpointed `-wal` or a `-shm`, except as part of exceptional recreation of a proven-corrupt index (§6).
- **Outward outcomes and retries:**

| SQLite outcome | Outward result | Retry |
|---|---|---|
| `SQLITE_BUSY`, `SQLITE_BUSY_RECOVERY` | `storage_busy` | bounded retry within the operation's deadline, as today |
| `SQLITE_BUSY_SNAPSHOT` (a read transaction's snapshot can't be upgraded or continued) | `storage_busy` if the restart budget is exhausted | end that read transaction and restart the read on a new snapshot within its deadline; never spin inside the old transaction |
| `SQLITE_READONLY_RECOVERY`, `SQLITE_READONLY_CANTINIT` on a read-only connection | `storage_busy` (recovery needs the worker's writable open) | the read retries after the worker has opened writable; existing-only probes (§4) report `busy` |
| `SQLITE_READONLY_DBMOVED` (the file was replaced or unlinked under the connection) | after one reopen: the result of the reopened read, or `storage_busy` | discard the connection and reopen once; a second `DBMOVED` is `storage_busy` |
| `SQLITE_FULL`, `SQLITE_IOERR*`, permission errors | the existing typed storage failures | as today; never authorizes deletion (T06) |
| `SQLITE_CORRUPT`, `SQLITE_NOTADB` | the existing corruption path | may lead to exceptional recreation (§6) |

- **Sidecar rules:**
  - `reject_sidecars` accepts `-wal`/`-shm` for `index.db`;
  - a `-journal` beside an `index.db` is handled only by §2's probe, never served past.
- **No separate descriptors:** the rule against closing a separate descriptor on a live SQLite inode applies to the WAL and shm files too. Size checks use path metadata, never an opened descriptor.

### 4. Existing-only opens

In WAL mode, a read-only SQLite open can **create** `-wal` and `-shm` (`../../mcp-pilot-slice0-findings.md` §2). Every direct existing-only opener of `index.db` follows one of these rules.

- **Daemon probes of a checkout with no runtime.** Two probes, both run in the daemon:
  - `Store::status_existing_readonly` (the prior-head probe);
  - `Store::browser_index_root_existing` (browser discovery).

  Rules:
  - If a runtime exists for the checkout, they read through it.
  - Otherwise, if `-wal` or `-shm` exists, they report `busy` without opening SQLite, as status already does today.
  - Otherwise they hold the registry's activation fence for that key, so the only possible writer can't start during the probe, and open the index read-only with `immutable=1`. That creates no sidecars. They release the fence afterwards.
- **`readonly_index_db`** is split into an index variant and a queue variant. The queue variant keeps today's rollback-mode checks for `requests.db`.
  - **`trellis gc --report`** (CLI, not the daemon) uses the index variant and writes nothing. If `-wal` or `-shm` exists, or verified non-blocking exclusive index use isn't available, the entry is reported `busy`. Otherwise it reads `immutable=1` under that exclusive use.
  - **Orphan-queue inspection** (daemon idle exit) can't let a crash-left WAL make its answer permanently uncertain, because "uncertain means pending" would block idle exit forever. If a `-wal` exists and verified exclusive index use is available (no live opener), the daemon, as the sole writer, performs SQLite recovery and a `TRUNCATE` checkpoint through a writable open, closes, and then inspects. If exclusive use isn't available, the answer is pending and retried, as today.

### 5. Checkpoints and the WAL threshold

- **Who checkpoints:** `wal_autocheckpoint=0` on **every writable `index.db` connection**. The checkout worker owns checkpointing.
- **After each COMMIT,** as worker maintenance:
  1. a `PASSIVE` checkpoint;
  2. then a `TRUNCATE` checkpoint with a bounded busy wait, attempted when no reader holds an older snapshot.

  Each result's `(busy, log frames, checkpointed frames)` is inspected. A partial `PASSIVE` or a failed `TRUNCATE` is **incomplete maintenance**: it's reported in status and retried, never recorded as success.
- **Failure is maintenance failure only:** a checkpoint failure after COMMIT never rolls back, fails or changes a committed request. Terminal outcomes stay monotone, and accepted rows stay durable.
- **A soft threshold, not a ceiling.** One atomic publication can write ≈2 GB of WAL at 10k files, and a reader holding an old snapshot prevents reset. Trellis promises neither a WAL or disk ceiling nor unconditional progress for new writes alongside an arbitrarily long reader.
  - It applies a **soft next-publication threshold** (the budget, an owner decision; the draft proposes 4 GiB).
  - **Below the threshold,** work is admitted, and a publication may overshoot it.
  - **At or above it,** the worker first runs a bounded `TRUNCATE` as part of preparing that work. This is not lowest-priority maintenance, so it can unblock the waiting publication. If the WAL is still at or above the threshold:
    - **A queued FIFO request** is not claimed. It stays `queued`, durable and first in FIFO order, and the claim retries with typed backoff. The check runs **before** the claim; a request already claimed and `running` is never returned to `queued`, and its publication proceeds.
    - **Startup reconciliation (H)** defers its publication with typed backoff. It keeps holding FIFO behind it, as Decision 0009 requires, and status reports why.
    - **Watcher work** defers. Its hints coalesce, and the Q2→watcher→Q1 order is unchanged.
  - Every Trellis read has a deadline (#142; MCP 5 s), so only a stuck handle holds a snapshot indefinitely, and that is a defect to report.

### 6. Staged replacement and GC

- **Exceptional recreation (T06), for an obsolete (§2) or proven-corrupt index:**
  1. The stage is built in WAL, `TRUNCATE`-checkpointed, and has every connection closed. Path metadata must then show no stage `-wal` or `-shm` (SQLite removes them on last close).
  2. Under the verified exclusive use T06 requires, the old index's connections are closed.
     - **Healthy old WAL:** an obsolete old index has none. A healthy WAL old index is `TRUNCATE`-checkpointed and closed, and its `-wal`/`-shm` must be absent.
     - **Proven-corrupt old index:** its uncheckpointable `-wal` and `-shm` belong to the corrupt incarnation. They are unlinked together with it, under the same exclusive use and corruption classification, and never in any other case.
     - Ordinary busy or I/O checkpoint failures defer the replacement; they never unlink.
  3. The rename over `index.db` follows, then a directory fsync.
  - **A failed rename** leaves the old index in place and reports failure.
  - **A successful rename followed by a failed directory fsync or validation** leaves the durable namespace uncertain. The checkout enters the existing recreate-pending state and reports failure, never success.
  - **On the next activation,** the §2 probe revalidates whatever file is at the pathname:
    - if it's the staged generation and validates, it's adopted;
    - otherwise it's classified obsolete or corrupt and recreated again.

    Before adopting it, path metadata must show no `-wal`/`-shm` that predates the new inode. A stale sidecar is never attached to the new file.
- **GC (Decision 0006 §2):**
  - **Skip condition:** "skip anything … with a hot journal" becomes "skip anything with a `-journal`, or with a `-wal` the worker can't `TRUNCATE`-checkpoint to empty under verified exclusive use". An ordinary live WAL is not a hot rollback journal: it means the index is live, and GC skips live entries anyway.
  - **Unchanged:** fresh eligibility checks, inventory, the `requests.db` handling and the lock ordering of T06 and Decision 0006.
  - **Before the first unlink,** any failure skips the entry. After the first unlink, an I/O or fsync failure is a typed **incomplete removal**: it's reported as a failure, never as success or a skip, and the remaining files are retried.

### 7. Snapshots

- **Per read:** each read keeps one SQLite read transaction for its snapshot, as today.
- **Fences:** `data_version` fences stay on the same connection.
- **Connections don't share snapshots:** two connections reading the same pin don't automatically see the same snapshot. Any design that reads one revision on several connections (#199 Q-6) must establish that itself.
- **Unchanged:** selected-document attestation and read deadlines.

## Required tests (hook-synchronized, per CLAUDE.md)

1. **Reads during a real publication:** a validated prior pin stays readable while it runs, through the browser, selected source and an MCP read. A fresh new head is read after COMMIT.
2. **Kill before COMMIT** (incomplete WAL frames): reopen yields the old committed pin, and queue FIFO reconciles with no false ACK and no terminal state moving backwards.
3. **Kill after a durable COMMIT, before checkpoint and queue completion:** reopen yields the new committed pin, and the request completes once with the right outcome.
4. **Old reader held through commits:** hold an old-snapshot reader through several commits and a partial checkpoint; status shows incomplete maintenance. Release it, and the `TRUNCATE` completes.
5. **Threshold:**
   - below the threshold, a publication that overshoots it completes;
   - at or above it with a held reader, a queued request stays `queued` and is claimed after release;
   - a `running` request is never re-queued;
   - H and watcher work defer and resume in order.
6. **Outcome table (§3):** each outcome maps as stated, injected into recovery, reads and checkpoints, including `BUSY_SNAPSHOT` restarting the read transaction. A checkpoint fault never fails a committed request.
7. **Obsolete probe (§2):**
   - a healthy old index is recreated, keeping `requests.db`;
   - after a killed old writer, the hot `-journal` is rolled back before classification;
   - busy exclusive use defers;
   - a malformed or unknown mode or marker never leads to an unsafe deletion;
   - a changed inode at recheck abandons the replacement.
8. **Replacement:**
   - a rename failure leaves the old index;
   - rename-then-fsync failure enters recreate-pending and revalidates on the next activation;
   - a corrupt old WAL is disposed only on the corruption path;
   - no stale sidecar attaches to the new inode.
9. **GC:** skips a live or uncheckpointable WAL, and reports a post-unlink failure as incomplete removal.
10. **Existing-only opens (§4):** `status_existing_readonly`, `browser_index_root_existing` and `gc --report` never create sidecars and report `busy` as specified. Orphan inspection recovers a crash-left WAL under exclusive use and doesn't block idle exit indefinitely.

Existing tests specific to the rollback journal, such as the `daemon_crash_recovery.rs` hot-journal kills, are replaced by 2 and 3 for `index.db`, and are kept as they are for `requests.db`.

## Open owner decisions

1. Adopt WAL for `index.db` as scoped here.
2. The soft threshold's value (§5).

## Consequences

- **Reads stay available:** a publication no longer blocks prior-head reads, under the conditions in §1.
- **Disk use:** grows by the WAL between checkpoints. It is usually reset after each publication, can overshoot the soft threshold for one publication, and stays larger while a reader holds an old snapshot.
- **New worker unit:** checkpoint maintenance, with status reporting.
- **More states to handle:** the recovery, sidecar, existing-only, stage and GC code gains WAL-aware states. This is some of the most closely reviewed code in the repository, so it's medium-to-large work, scheduled after #199's in-flight items.
