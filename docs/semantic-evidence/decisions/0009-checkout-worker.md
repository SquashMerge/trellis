# Decision 0009: one event-driven worker per checkout

- **Status:** draft for owner ratification, 2026-10-14. Ratified when this record merges. It governs #138, implemented in PR #139.
- **What it amends:**
  - Decision 0008 §1 and §3: the name "H" becomes **startup reconciliation**, and its owner becomes the checkout worker;
  - Decision 0007 §3: release mechanics. The 15- and 30-minute policies are unchanged;
  - `../../mcp-readonly-pilot-contract.md`: when a call is admitted, how it is bounded, and how the relay carries concurrent calls.
- **What it leaves unchanged:**
  - Decision 0008's single writer, its recovery order (open queue → settle old-root requests with a confirmed COMMIT → startup reconciliation → FIFO), its crash guarantees and its read rules (§4);
  - FIFO order and the Q2→watcher→Q1 gate;
  - pins, retention and GC eligibility (Decision 0006);
  - validated-head reads and selected-document attestation;
  - MCP error precedence and the closed tool schemas, except where §7 says otherwise.
- **Compatibility:** none needed (pre-release).

## Background

Inside the daemon, a checkout's work is coordinated by several actors that poll and probe each other's locks:
- a 20 ms queue tick and a 20 ms maintenance tick;
- a separate startup-reconciliation task that can race the queue tick's own reconciliation path;
- a registry that retires runtimes by `try_lock` on the ticks' lanes;
- `serve`, which retires and replaces a CLI-activated runtime and waits on its accepted work;
- CLI waits that poll `requests.db`;
- a 250 ms lifecycle poll that does SQLite work under the registry lock.

#138 inventories 25 such sites. #122's intermittent failures (the serve stall, release starvation, health samples) trace back to them.

## Decision

### 1. Worker authority

Each active checkout has **one worker**, a dedicated thread. It is the only actor that does any of the following for that checkout:
- startup reconciliation (formerly "H"), including after root loss and recreate;
- old-root request settlement, `retire_lost_root`, and every other recovery path;
- index publication, FIFO claims and request completion;
- watcher reconciliation and periodic inventory;
- retention and GC units;
- retirement.

No handler, registry operation or test path publishes, claims or reconciles outside the worker. Tests drive it through a synchronous single-step entry point that runs the same code.

### 2. Runtime states

`Activating → Reconciling{attempt} → Ready(session) → RetireRequested{cutoff} → Retired`

`RootLost` is reachable from `Reconciling`, `Ready` and `RetireRequested`.

| Transition | Owner | Trigger |
| --- | --- | --- |
| `Activating → Reconciling` | registry | `activate` after the queue is opened and prepared |
| `Reconciling → Ready` | worker | Startup reconciliation commits a validated head |
| `Reconciling → Reconciling{attempt+1}` | worker | Failure; retried with backoff (250 ms doubling, at most 30 s) |
| `Ready → Reconciling` | worker | Writer session lost, recreate pending, or a non-retryable drain failure |
| `Ready → RetireRequested{cutoff}` | registry | Release due (§5) and accepted by the worker's decline check |
| `RetireRequested → Ready` | registry | Client reattach, or a FIFO enqueue; atomic under the state lock |
| `RetireRequested → Retired` | worker | Retirement completes (§5) |
| any of the above → `RootLost` | worker | Root missing or replaced; old-root requests settled with a confirmed COMMIT |
| `Retired` → removed | registry | On the worker's `Retired` event. A later `activate` creates a fresh runtime |

The state sits behind one mutex shared by the registry and the worker, and is published through a `watch` channel.

### 3. Published status is advisory, never authority

Readers use the published snapshot for **status only**: state, `catchingUp`, FIFO pending, accepted versus acknowledged watcher generation, watcher health, and last error.
- **Reads** still go through validated-head admission and selected-document attestation, with the root checked before and after materializing.
- **Old-root settlement** still requires a confirmed terminal COMMIT.
- **`requests.db`** remains the record of every request outcome.

The snapshot replaces lock probing as information. It never replaces the checks above.

In `Reconciling` and `RetireRequested`, reads keep serving the last validated head (Decision 0008 §4). A worker does not drop its read access to that head until `Retired`.

### 4. Priority at each worker boundary

The worker runs one unit at a time. Before each unit it picks the highest class that has work:
1. **Startup reconciliation**, while `Reconciling`. FIFO is held until it commits.
2. **Accepted FIFO and watcher work**, interleaved exactly as today: one FIFO row, then the watcher gate (watcher work runs when the FIFO is empty or a row just completed).
3. **Retirement**, while `RetireRequested`.
4. **Maintenance and GC**, chunked. Each unit yields when a higher class has work: the worker's own pending flags are checked, and the publication gate's waiting count is a published atomic.

Units are never preempted mid-way. Accepted work is never cancelled to make room for lower classes.

**Wakeups.** The worker blocks on its command channel with a timeout set to its next deadline:
- watcher debounce (75 ms quiet, 250 ms maximum);
- the 60 s periodic inventory;
- backoff;
- the maintenance cadence.

There is no interval timer.

### 5. Retirement

**When a release is due** (Decision 0007 §3 policy), the registry asks the worker to retire. The request is **declined**, and the runtime stays `Ready`, while any of these hold:
- a FIFO row is queued or running;
- startup reconciliation is pending;
- an MCP or CLI read lease is held.

Accepted FIFO work and its Q2→watcher→Q1 gate are never skipped.

**When accepted**, the worker:
1. records the watcher's accepted generation at that moment as the **cutoff**;
2. finishes watcher work up to the cutoff;
3. closes the watcher and the session;
4. retires the SQLite witnesses;
5. publishes `Retired`.

**Changes after the cutoff are disposable.** Changes accepted after the cutoff are not reconciled by this runtime: any later activation runs mandatory startup reconciliation, which captures them. So retirement completes under continuous file churn, and nothing is lost.

**What cancels a retirement:**
- a client reattach;
- a FIFO enqueue, including a browser `/index`.

Either one, while in `RetireRequested`, returns the runtime to `Ready` atomically.

**Serve never retires a runtime** (§6).

**The lifecycle** is event-driven: client disconnects, browser leases and retirements wake it, and it sleeps until its next deadline. It does no SQLite work under the registry lock. Daemon exit still requires every runtime to be removed and no clients.

### 6. In-place serve registration

`serve` registers its browser options on the **running** runtime:
- **Build.** The whole browser configuration (source directory, Rust source roots, dependency catalog options, Jev and ACP providers, explicit index options) is built and validated outside the worker, all or nothing.
- **Publish.** It is published as one immutable, versioned value. Serve is acknowledged **when that version is published**, without waiting for any worker boundary.
- **Failure.** A failed build refuses the registration and publishes nothing; the previous version stays.
- **Accepted requests keep their options.** An accepted request runs with the options recorded with it in `requests.db`. The watcher and later requests use the new version's index options; the watcher's change to new options forces a fresh full inventory, as today.
- **Conflicts.** A *different* existing registration still conflicts.

### 7. MCP calls

- **Admission.** A `tools/call` is admitted when the daemon session accepts it. Its five-second deadline starts then and covers every later stage: workspace selection, readiness, the read, response construction and the send decision.
- **Concurrency.** Up to four admitted calls per connection run concurrently. A fifth gets `too_many_requests` at once, without queueing.
- **Worker budget.** Each call runs on a worker thread that holds a permit until the thread exits, including after its deadline passes or it is cancelled. A **daemon-wide budget** of 16 such threads bounds work across connections and reconnects. A call that can't get a permit gets `too_many_requests`. A failure to start a thread is a typed `store_unavailable`, never a panic.
- **Send decision.** At the serialized send decision, the coordinator rechecks:
  1. cancellation;
  2. the deadline;
  3. the selected root's identity.

  A worker's earlier root check doesn't substitute for this. A late result after the deadline becomes `deadline_exceeded`. Cancellation overrides all unsent outcomes.
- **The relay pipelines calls** (owner ruling 2026-10-14):
  - It forwards each call as it arrives and routes replies by request ID.
  - A cancellation is forwarded to the daemon rather than closing the connection.
  - The #127 decision point applies per request ID.
  - The relay's own queue goes away, so "without queueing" holds end to end.
- **Unresolved failures** (owner ruling 2026-10-14). A call that is refused or times out before its workspace resolves gets `too_many_requests` or `deadline_exceeded` in a pre-resolution failure shape that carries `attemptedWorkspace`, like selection failures, and no `workspace` or `catchingUp`.

### 8. Signals carry no truth

- **Watcher hints** may be coalesced or dropped. The watcher's generation counter is incremented before any hint is sent. Overflow still forces a full inventory. On every wake, the worker re-reads the generation, the overflow flag and the `requests.db` head.
- **Commands are lossless and acknowledged:** retire, cancel-retire and shutdown.
- **Request-completion waiters:**
  - subscribe before re-reading `requests.db`;
  - re-read after every wake;
  - re-subscribe if the runtime is replaced.

  The notifier fires only after a confirmed terminal COMMIT, at every terminal site.
- **Watcher health** updates carry the watcher's epoch, so a callback from a replaced watcher can't mark the current one degraded.

## Owner rulings (2026-10-14)

1. **Relay pipelining (§7): pipeline.** The relay used to forward one call at a time and queue up to 16 more frames, so calls waited, unbounded, before the daemon admitted them, and four-way concurrency could not happen through a real agent client. Pipelining makes the contract's "four concurrent, fifth refused without queueing" and the deadline-from-admission rule hold for agents. The rejected alternative was a serial relay acting as the admission point, which would still run calls one at a time.
2. **Failure shape before resolution (§7): a new pre-resolution shape** carrying `attemptedWorkspace`. The rejected alternative, reporting the launch workspace in the resolved shape, would misattribute a call that explicitly selected another worktree.

## Consequences

- **What goes away:** the polling ticks, lane mutexes, lock-probe priority checks, the registry's `try_lock` release, serve's replacement wait, and the CLI's request polling.
- **Retirement can't starve:** it is the worker's own work item.
- **Serve answers without waiting** for index work.
- **MCP concurrency** is real, and its deadline is measured from admission.
- **Tests** await events and worker pause points instead of sampling lock state; functional tests assert no speed.
- **Checklist:** #138's inventory is ticked off item by item in #139, each item marked done, implemented in a named commit, or deferred with a reason.
