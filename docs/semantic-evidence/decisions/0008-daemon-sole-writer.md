# Decision 0008: the user daemon is the only writer

- **Status:** owner-approved 2026-10-10. Ratified when this record merges. It governs #122, the follow-up to #107 (PR #117).
- **What it amends:**
  - Decision 0007 §1: standalone CLI, and the daemon as follower;
  - #16's leader lifecycle;
  - `../../local-topology.md`: the leader, reader checks and takeover sections;
  - `../../mcp-readonly-pilot-contract.md`: the eligibility table.
- **What it leaves unchanged:**
  - per-checkout indexes, pins and revisions;
  - retention, and GC eligibility (Decision 0006). Only GC's lock mechanism may change, as §2 sets out;
  - the #67 request queue and its ordering;
  - the evidence contracts;
  - per-call workspace selection and idle timeouts (Decision 0007 §2–3).
- **Compatibility:** none needed (pre-release).
- **Amended by [Decision 0009](0009-checkout-worker.md):** one worker per checkout owns everything §1 assigns to the daemon, and serve registers in place instead of replacing a runtime. **Terminology:** "mandatory H" here is the worker's **startup reconciliation**; the rules are unchanged.

## Background

#16 was designed before the daemon existed: any Trellis process (CLI, MCP server, `serve`) could index a checkout, and a per-checkout leader lock elected one. Decision 0007 added the user daemon but kept that model. CLI commands still work standalone when the daemon isn't running, and the daemon stays a follower for a checkout whose lock a standalone CLI holds.

Two writers can therefore still compete for a checkout, and every handoff between them needs dedicated machinery:
- leader election and incarnation markers;
- follower mode;
- successor catch-up with prior-head reads;
- predecessor read permits;
- root-loss owner leases;
- cross-process `storage_busy` handling.

PR #117 shows the cost: cross-process ownership transitions contributed substantial complexity and failures to its later revisions. One developer, with the daemon starting on demand, doesn't need more than one writer.

## Decision

### 1. One writer

- **The user daemon is the only process that writes** a checkout's derived index (`index.db`) and request queue (`requests.db`). It is also the only process that runs watchers, mandatory H, publication, retention, release and GC.
- **Every command that touches index or queue data goes through the daemon**, starting it on demand as `trellis mcp` already does. That covers `index`, `serve`, `mcp`, `status`, `symbols`, `query`, `export`, `gc --report`, and browser writes such as saved views and notes.
- **There is no standalone fallback**, for reads or writes, so there is one read path.
  - Connect-or-start is bounded. If the daemon can't be reached or started within that bound, the command fails with a typed `daemon_unavailable` error. It never opens the index itself.
  - Commands that touch no index data, such as `--help` and `--version`, don't start the daemon.

### 2. One cross-process writer-election lock

- The daemon's existing **single-instance lock** (per user data directory) is the only cross-process **writer-election** lock. Index-use and durable-record locks are covered below.
- These are removed:
  - the per-checkout leader lock;
  - incarnation markers (`reconciled_incarnation`) as an ownership or read gate;
  - follower mode;
  - takeover and successor handoff.
- Checkout ownership becomes **in-process state** in the daemon's registry.
- **Election-path loss (#130, 2026-10-23):** a daemon exits when its retained election lock no longer matches the named lock (device/inode), including a removed data directory. A 250 ms observation interval bounds this liveness check; it is not a writer fence and does not claim to prevent overlap before loss is observed. Shutdown removes only the predecessor's socket, never a successor's.
- **Per-checkout use locks** may be removed only after both of these:
  1. every **Trellis-controlled** out-of-process `Store` or SQLite opener is removed or blocked (external tools are unsupported; see Consequences);
  2. a **daemon-local admission gate** replaces the lock. The gate must cover every active snapshot, every retained SQLite handle and every new opener, and stay held from GC's eligibility check through the unlink.

  A registry lookup alone isn't enough to protect GC. Decision 0006 §2's nonblocking exclusive use lock stays until this gate is proved; the gate then supersedes only that lock mechanism, not GC eligibility.
- **The separate durable-record lock** (saved views, notes, ledgers) stays unless its removal gets its own proof of safety.

### 3. Recovery: a new daemon instance, or a live root replacement

When a daemon instance activates a checkout, **or a running daemon detects that a checkout's root was replaced at the same path**, it does the following, in order, before any new-root H or claim:
1. **Open the queue.** SQLite rolls back any write a crashed instance left half-finished (hot journal) when the file is opened.
2. **Settle old-root requests.** In one serialized write transaction, mark every accepted (`queued` / `running`) request whose recorded root `(device, inode)` differs from the current verified root as terminal `root_changed`.
   - That COMMIT must be **confirmed** before H or any claim proceeds.
   - If the outcome is ambiguous (for example busy or an I/O error at COMMIT), the checkout stops making progress until a re-read of the queue confirms which rows are terminal.
3. **Run mandatory H.** It must commit a validated head before the first FIFO claim. Valid prior-head reads stay available during H (§4).
4. **Only then claim FIFO work.**

A crash at any point simply repeats these steps on the next activation. No **durable** cross-process handoff or quarantine state is kept, and no owner lease. A live daemon still blocks H and claims until the queue outcome is verified, or until recovery restarts.

**What a crash guarantees:**
- **Every accepted request ID in a retained queue that the daemon activates or services reaches exactly one durable terminal result** (`done` or `failed`).
- A queue with no activating client is not serviced. Its rows stay pending until the daemon next activates that checkout, or until eligible GC deletes the index.
- The queue (`requests.db`) lives in the derived index directory. When GC deletes an eligible index under Decision 0006 §2 (root missing or replaced, or 30 days unopened), any request still pending in that queue is abandoned with it. A fresh lookup of that ID afterwards returns a typed `not_found`. A client already waiting with a captured root that is now lost still gets `root_changed` (T04). Decision 0006's GC policy is unchanged; in practice, those requests could never run (root gone) or have had no waiter for 30 days.
- This is not a promise of one execution or one revision: a crash between committing an index revision and completing the queue row can redo the work, which can produce an additional revision.
- FIFO order and pin correctness still hold. A completed row's pin names a committed, validated revision, and no row is claimed out of order.

### 4. Reads (#111, unchanged in substance)

- **When it applies:** on **every** daemon-routed surface, including during cold start and catch-up after a daemon restart.
- **What a read serves:** the last committed, validated head — the current pin, a retained pin, or an unpinned read — whenever the opened root still matches its captured `(device, inode)` before and after the response is materialized. The answer carries `catchingUp` while activation, H, watcher work or queued work is pending.
- **Errors:**
  - `index_not_ready` only when there is no valid published head;
  - `root_changed` on root loss.
- **What is still refused:** evidence that fails validation, evidence that belongs to another checkout or root, and evidence for a lost root.
- **There is one read path**, and one `catchingUp` computation.

### 5. SQLite access

Only the daemon opens these databases, so no other supported Trellis process contends for them. The daemon's own connections can still contend with each other: reader snapshots against writer transactions, and maintenance against publication.
- **Contention stays typed and retried** (`storage_busy`, bounded) as AGENTS.md requires. Reduce it where that's simple, for example by serializing writers per checkout. Durability is unchanged (DELETE journal, FULL sync).
- **Keep the retained-FD rule** from #67: never close a separate file descriptor on a live SQLite inode. Retire only the parts of the handle cache that are provably unused.

## Consequences

- **Much less code.** Leader election, follower mode, takeover, root-loss owner leases and cross-process busy and retry paths are deleted. The cross-process handoff read permits are deleted and replaced by daemon-local admission of the validated prior head. The implementing PR must show a clear net reduction in production code.
- **The daemon is required.** Every data command needs a daemon, started automatically; if it can't start, the command fails with `daemon_unavailable`. A daemon crash briefly interrupts every session until clients restart it; accepted queue rows survive (Decision 0007 already accepts this).
- **Tests change.** Tests that simulate two competing Trellis processes on one checkout are replaced by daemon-restart and crash-recovery tests.
- **External tools.** Opening a live daemon index with an external tool such as the `sqlite3` CLI is **unsupported**. It may still physically succeed, so it isn't excluded. §2's gate covers Trellis-controlled openers only; external access while the daemon runs is unsupported and may observe or disturb in-progress state.
