# Decision 0009: one event-driven worker per checkout

- **Status:** owner-approved for provisional implementation, 2026-10-14. Ratified when PR #139 merges. It governs #138, implemented in that PR. See [Approval](#approval).
- **What it amends:**
  - Decision 0008 §1 and §3: the name "H" becomes **startup reconciliation**, and its owner becomes the checkout worker;
  - Decision 0007 §3: release mechanics. The 15- and 30-minute policies are unchanged;
  - `../../mcp-readonly-pilot-contract.md`: the protocol table, envelopes, admission, limits, precedence and the relay decision point. That document is amended in the same change.
- **What it leaves unchanged:**
  - Decision 0008's single writer, its recovery order (open queue → settle old-root requests with a confirmed COMMIT → startup reconciliation → FIFO), its crash guarantees and its read rules (§4);
  - FIFO order and the Q2→watcher→Q1 gate;
  - the causal meaning of the `H_READY` and `WATCH_ACK` witness events, whatever their names become;
  - prior-head read eligibility;
  - pins, retention, and GC eligibility and its use-lock protection (Decisions 0006 §2 and 0008 §2);
  - validated-head reads and selected-document attestation.
- **Compatibility:** none needed (pre-release).

## Background

Inside the daemon, a checkout's work is coordinated by several actors that poll and probe each other's locks:
- a 20 ms queue tick and a 20 ms maintenance tick;
- a separate startup-reconciliation task that can race the queue tick's own reconciliation path;
- a registry that retires runtimes by `try_lock` on the ticks' lanes;
- `serve`, which retires and replaces a CLI-activated runtime and waits on its accepted work;
- CLI waits that poll `requests.db`;
- a 250 ms lifecycle poll that does SQLite work under the registry lock.

#138 inventories 25 such sites. #122's intermittent failures (the serve stall, release starvation, health samples) trace back to them. The MCP relay also forwards one call at a time with an unbounded wait before daemon admission, so the contract's concurrency and deadline rules don't hold for agent clients.

## Decision

### 1. Worker authority

Each active checkout runtime has **one worker**, a dedicated thread. It is the only actor that does any of the following for that runtime:
- startup reconciliation (formerly "H");
- old-root request settlement and every other recovery path;
- index publication, FIFO claims and request completion;
- watcher reconciliation and periodic inventory;
- retention and GC units;
- retirement.

No handler, registry operation or test path publishes, claims or reconciles outside the worker. Tests drive it through a synchronous single-step entry point that runs the same code.

### 2. Runtime states

`Activating → Reconciling{attempt} → Ready(session) → RetireRequested{cutoff} → Closing → Retired`

`RootLost` is reachable from `Reconciling`, `Ready` and `RetireRequested`.

**Readers in each state:**

| State | Reads | New work |
| --- | --- | --- |
| `Activating` | `index_not_ready` until the queue is open | Enqueue accepted durably; claimed only after startup reconciliation. A daemon runtime leaves this state at activation. A standalone test scheduler, which has no registry, starts startup reconciliation when work is accepted |
| `Reconciling` | Last validated head, `catchingUp:true`; `index_not_ready` if there is none | Enqueued; FIFO held |
| `Ready` | Current validated head | Claimed in FIFO order |
| `RetireRequested` | Last validated head | Reattach, a new read lease, or an enqueue cancels the retirement (§5) |
| `Closing`, `Retired` | Not admitted on this runtime. The caller waits for the `Retired` event and activates a fresh runtime | Goes to the fresh runtime |
| `RootLost` | `root_changed`; old-root evidence is never served | Accepted requests are settled `root_changed` |

**Transitions:**

| Transition | Owner | Trigger |
| --- | --- | --- |
| `Activating → Reconciling` | registry | The queue is opened and prepared at activation |
| `Reconciling → Ready` | worker | Startup reconciliation commits a validated head |
| `Reconciling → Reconciling{attempt+1}` | worker | Failure; retried after backoff (250 ms doubling, at most 30 s) |
| `Ready → Reconciling` | worker | Writer session lost, recreate pending, or a non-retryable drain failure |
| `Ready → RetireRequested{cutoff}` | registry, then worker | Release due (§5), and not declined |
| `RetireRequested → Ready` | registry or enqueue gate | Reattach, a new read lease, or an enqueue; atomic under the state lock (§5) |
| `RetireRequested → Closing` | worker | Linearization point: the cutoff work is done and irreversible close begins |
| `Closing → Retired` | worker | Watcher released (no authority; native teardown may still run, see §5), session, SQLite witnesses and use locks closed |
| `Reconciling`/`Ready`/`RetireRequested` → `RootLost` | worker | The root is missing, or the path now has a different `(device, inode)`, **and** the old-root settlement COMMIT is confirmed. Until then the runtime stays in its state, keeps its session and watcher, and makes no other progress. Its reads already fail `root_changed` at the root check. After an ambiguous outcome (busy, I/O error at COMMIT), the worker retries settlement at each boundary until a re-read confirms the terminal rows |
| `RootLost → Closing` | worker | Immediately after settlement |
| `Retired` → removed | registry | On the worker's `Retired` event |

**Recovery from a live root replacement** (Decision 0008 §3) completes through a **fresh runtime**. The old runtime settles its old-root requests, then closes. The next attach or durable enqueue for the path activates the replacement root as a new runtime. That runtime repeats Decision 0008 §3 from the queue open: settle any remaining foreign-root rows, run startup reconciliation, then claim FIFO. A runtime is never re-pointed at a different root.

The state sits behind one mutex shared by the registry, the enqueue gate and the worker, and is published through a `watch` channel.

### 3. Published status is advisory, never authority

Readers use the published snapshot for **status only**: state, `catchingUp`, FIFO pending, accepted versus acknowledged watcher generation, watcher health, and last error.
- **Reads** still go through validated-head admission and selected-document attestation, with the root checked before and after materializing.
- **Old-root settlement** still requires a confirmed terminal COMMIT.
- **`requests.db`** remains the record of every request outcome.

The snapshot replaces lock probing as information. It never replaces the checks above. A runtime keeps read access to its last validated head until it enters `Closing`.

### 4. Priority at each worker boundary

The worker runs one unit at a time. Before each unit, it picks the highest class that has work:
1. **Startup reconciliation**, while `Reconciling`. Old-root settlement while `RootLost`. FIFO is held until a validated head commits.
2. **Accepted FIFO and watcher work**, interleaved exactly as today: one FIFO row, then the watcher gate. Watcher work runs when the FIFO is empty or a row just completed.
   - **In `RetireRequested`**, watcher intent **through the cutoff** gets one attempt before retirement (§5). Changes after the cutoff don't outrank retirement and can't delay it.
3. **Retirement**, while `RetireRequested`.
4. **Maintenance and GC**, chunked. Each unit yields at its next safe point when a higher class has work; it reads the worker's own pending flags and the publication gate's published waiting count. GC of other, inactive checkouts' indexes is a maintenance unit too. Its eligibility and use-lock protection are unchanged: a runtime's index keeps its use lock until that runtime has finished `Closing`.

Units are never preempted mid-way. Accepted work is never cancelled to make room for lower classes.

`WATCH_ACK` acknowledges a generation only after a verified capture that covers it. After a change of options, it acknowledges only after the full inventory for the new options completes.

### 5. Retirement

**When a release is due** (Decision 0007 §3 policy), the registry asks the worker to retire. The request is **declined**, and the runtime stays `Ready`, while any of these hold:
- a FIFO row is queued or running;
- startup reconciliation is running (not when a failed attempt is waiting out its backoff: an idle checkout whose startup keeps failing may retire, and the next client starts over), or old-root settlement is pending;
- a read lease is held.

Accepted FIFO work and its Q2→watcher→Q1 gate are never skipped.

**When accepted**, under the state lock, the worker stamps the **cutoff**: the watcher's accepted generation at that moment. It then:
1. makes one forced attempt at watcher intent up to the cutoff, if any is unacknowledged. Its outcome, including a capture invalidated by later changes, never holds the retirement: the next activation's startup reconciliation captures whatever it missed;
2. takes the state lock again, and enters `Closing` only if the state is still `RetireRequested`;
3. releases the watcher, then closes the session, the SQLite witnesses and the use locks;
4. publishes `Retired`.

**Releasing a watcher** (amended 2026-10-15). A released watcher has no authority over the checkout: once the release returns, its callbacks do nothing (no health, no events, no wakes); a callback already past its check when the release starts finishes first. Its native teardown runs on a background reaper, because a native library's shutdown is not relied on to complete: notify's macOS FSEvents stop can be lost and never return. Every native watcher holds a slot from before its creation until its teardown returns, active or not, including one whose setup failed, so a lost stop strands at most the watchers that hold slots. At most **68** slots exist per daemon, so **up to 68 watchers can be stranded** if every teardown hangs; there is no separate limit of 4 stranded (one per active checkout, 64, plus 4 for replacement and stuck teardown); at that limit no new native watcher is created, and a checkout without one reports degraded watcher health and finds changes through the periodic full inventory (60 s). A watcher whose setup fails is released the same way.

**Degraded mode** (owner ruling, 2026-10-15). A checkout that cannot obtain a native watcher, because registration fails or the 68-slot budget is full, is a scoped exception: it exposes degraded watcher health and relies on the existing 60-second periodic full inventory. Its convergence and its parity with a full rebuild are reported separately. It is never claimed to meet the normal watcher latency targets, and no timer-to-publication guarantee is made for it. The Stage 3 watcher gate is unchanged: medium p95 ≤ 2 s and large p95 ≤ 5 s over the required 20 + 20 samples of **healthy native watching**, on the pinned final head and the quiet reference host. If watching degrades during a series, that series is invalid and is restarted once watching is healthy; degraded samples are never dropped to improve p95.

**Linearization.** Reattach, a new read lease and a durable enqueue all take the same state lock:
- **Before `Closing`:** they cancel the retirement, the runtime returns to `Ready`, and its resources are still live.
- **From `Closing` on:** they don't touch this runtime. They wait for its `Retired` event, which is bounded by the close, and then use a fresh runtime.

A half-closed runtime is never revived.

An enqueue through any `Store` counts as in flight from before its transaction until after its wake, so one that committed before `Closing` cancels the retirement even if its wake has not run. An enqueue that starts after `Closing` is durable and waits for the checkout's next activation. In-process clients always enqueue through the runtime's admission gate, which refuses from `Closing` on, so this applies only to direct `Store` use.

A runtime closed for root loss does not keep a verified replacement root from attaching, even while its stale clients stay connected. They keep getting `root_changed`.

**Changes after the cutoff are disposable.** This runtime doesn't reconcile them, and doesn't acknowledge them. Any later activation runs mandatory startup reconciliation, which captures them. So retirement completes under continuous file churn, and nothing accepted is lost.

**Serve never retires a runtime** (§6).

**The lifecycle** is event-driven: client disconnects, browser leases and `Retired` events wake it, and it sleeps until its next policy deadline. It does no SQLite work under the registry lock. Daemon exit still requires every runtime to be removed and no clients.

### 6. In-place serve registration

`serve` registers its browser options on the checkout without retiring its runtime. The daemon's provisioning mutex serializes serve registrations, which is the reservation: no other registration can start until this one finishes. One registry guard then covers, in order:

1. **Conflict check.** A *different* existing registration conflicts, and nothing changes. A checkout without a registration takes one in place, even while its runtime runs and clients are attached.
2. **Build.** Validate and build the complete browser configuration (source directory, Rust source roots, dependency catalog options, Jev and ACP providers, explicit index options) as one immutable value, all or nothing. This is bounded local work (directory and ledger opens), done under the guard as activation does. If this is the daemon's first browser registration, bind the listener and load and check the token after the build.
3. **Publish.** In one step under the same guard: record the registration and publish the new configuration version on the live runtime, if there is one. Without a live runtime, the next activation builds from the registration.

**Failure.** A failure at any step publishes nothing and returns the typed error, never success. Failures include a conflicting registration, a build failure, and a listener or token failure. A build failure is detected before any token is minted. The previous configuration stays.

**What the acknowledgement means.** Serve is acknowledged after step 3. That means the listener is ready and the configuration version is published; browser requests from then on use it. The acknowledgement doesn't mean the watcher has switched yet. The worker applies the new index options at its next watcher step, with a fresh full inventory (§4).

**Concurrent operations:**
- Attach and enqueue don't conflict with a registration.
- Each browser request pins one configuration version for its whole duration.
- An accepted request runs with the options recorded with it in `requests.db`.
- If no runtime is live, the next activation uses the published version.

### 7. MCP calls

**Two admission points:**
- **Protocol acceptance:** the relay, or a direct daemon-socket client's session, reads a complete valid `tools/call`. The call's **deadline** (five seconds) starts here.
- **Read admission:** the existing contract step for a resolved checkout. Application fields are validated, then the root is checked, before read admission and again at final eligibility. That ordering is unchanged.

**Order after protocol acceptance:**
1. **Application validation.** This is pure and runs first: an invalid call gets `invalid_request`, even at capacity.
2. **Capacity:**
   - **Per connection:** at most four outstanding calls. A call stops being outstanding once its outcome is written or suppressed. A fifth gets `too_many_requests` at once, without queueing.
   - **Daemon-wide:** at most 16 live call-worker threads. A worker keeps its permit until its thread exits, even after its call has timed out or been cancelled. A refusal for this reason is also `too_many_requests`, which is retryable. So does a failure to start a thread, never a panic.
3. **Workspace selection, then read admission and the read.** These run on the call's worker.
4. **The daemon's send decision**, on the connection's serialized writer. It rechecks, in order:
   1. cancellation;
   2. the deadline;
   3. the selected root's identity.

   An earlier root check on the worker doesn't substitute. A result decided after the deadline becomes `deadline_exceeded`.

Cancellation overrides all unsent outcomes. Otherwise `deadline_exceeded` overrides any outcome not yet decided at the deadline.

**The relay pipelines calls** (owner ruling 2026-10-14):
- **Forwarding:** it forwards each accepted call as it arrives. On the daemon connection it uses its own monotonic request IDs, mapped back to the client's IDs, so a late daemon reply can never match a client ID that was reused after its outcome was written.
- **Cancellation:** a cancel is forwarded to the daemon rather than closing the connection.
- **The client-facing deadline:** the relay enforces the strict reply bound for agents, measured from its own protocol acceptance, so it fires no later than the daemon's. The relay decides each call's outcome by acceptance + 5 s: the daemon's reply if it has arrived, otherwise `deadline_exceeded`.
- **The #127 decision point** applies per client ID to every decided outcome, including the relay's own `deadline_exceeded`: input the client finished writing before the decision is settled first, so a cancel among it suppresses the outcome. The settling wait is bounded and, for a call, ends by its deadline. If it can't settle in time, the outcome is `outcome_unknown`, as the existing rule says.
- **What the bound covers:** the decision. A client that stops reading stdout can delay the write itself.
- **Root checks:** the relay does none. The daemon's send decision is the final root check.
- **Connection loss:** for each outstanding call, the existing rule applies per ID. An interrupted read may be replayed once on the new connection, within its original deadline; otherwise it gets `daemon_unavailable`.
- **Non-tool requests** are forwarded in order. They are refused with `-32000` and `error.data.code:"too_many_requests"` only beyond a bound of 16 outstanding non-call requests per connection.

**Pre-resolution failure shape** (owner ruling 2026-10-14). A `too_many_requests` or `deadline_exceeded` decided **before** read admission sampled `catchingUp` (by the relay, at capacity, or before selection resolved) uses:

`{schemaVersion:1,requestId,error:{code:"too_many_requests"|"deadline_exceeded",message,retryable:true,attemptedWorkspace,currentBasis:null,currentContentHash:null}}`

- It has no `workspace`, `catchingUp` or `reason`.
- `attemptedWorkspace` uses the existing closed, bounded encoding:
  - the selected string, as `{kind:"path",value,utf8Bytes,truncated}`;
  - or, if the selection was omitted, the launch workspace path in the same form.
- An invalid-type or oversized `workspace` never reaches capacity, because validation runs first; it keeps its `invalid_request` shape.

After read admission, `deadline_exceeded` keeps the resolved failure shape.

### 8. Signals carry no truth, and wakes cannot be lost

**Wake.** Each worker has an inbox: one mutex-guarded record with a condition variable, holding:
- a sticky `wake` flag;
- a FIFO of commands: retire, cancel-retire, shutdown.

**Senders.** Every sender updates its durable or sticky condition first, then sets `wake` or pushes its command, then notifies the condition variable:
- watcher events increment the watcher generation, or set overflow;
- enqueues COMMIT their row;
- registrations publish their version.

Setting a flag can't fail, so a burst of hints coalesces into one wake and none is lost. Commands are queued without loss; each command is a one-per-state request, so the queue is bounded. Each command is acknowledged by the published state transition it causes.

**The worker's wait.** It sleeps on the condition variable until `wake` or a command is present, or its next deadline passes:
- the watcher debounce (75 ms quiet, 250 ms maximum);
- the 60 s periodic inventory;
- backoff;
- maintenance cadence.

A watcher event therefore wakes the worker immediately, and the worker computes the event's debounce deadline from the generation it reads. On every wake it re-reads the watcher generation, the overflow flag and the `requests.db` head.

**Request completion.** There is one monotonic **terminal-commit counter per queue identity**, process-wide. It follows the publication gate's pattern: a global map from the queue's root identity to a shared counter.
- **Who increments it:** every terminal writer, through any `Store` instance. That includes the request finish, old-root settlement, foreign-root settlement and the recreate paths. Each increments the counter once per transaction, **after** its COMMIT is confirmed. After an ambiguous COMMIT, the increment comes once a re-read confirms the outcome.
- **How a waiter uses it:**
  1. subscribe, recording the counter;
  2. re-read its row in `requests.db`;
  3. if the row isn't terminal, wait for the counter to change, then re-read.
- **Runtime replacement:** the waiter also watches the runtime state, so `Closing`, `Retired` or `RootLost` wakes it to re-check and re-subscribe on the new runtime.
- **No leaks:** there is no per-ID registration to leak; a waiter that goes away just drops its subscription.
- **Restarts:** a daemon restart ends every waiter. The client's existing `daemon_unavailable` / `outcome_unknown` rules apply, and the row in `requests.db` stays the answer.

**Watcher health** updates carry the watcher's epoch, so a callback from a replaced watcher can't mark the current one degraded.

## Owner rulings (2026-10-14)

1. **Relay pipelining (§7): pipeline.** The relay used to forward one call at a time and queue up to 16 more frames, so calls waited, unbounded, before the daemon admitted them, and four-way concurrency could not happen through a real agent client. Pipelining makes the contract's "four concurrent, fifth refused without queueing" and the deadline-from-acceptance rule hold for agents. The rejected alternative was a serial relay acting as the admission point, which would still run calls one at a time.
2. **Failure shape before resolution (§7): a new pre-resolution shape** carrying `attemptedWorkspace`. The rejected alternative, reporting the launch workspace in the resolved shape, would misattribute a call that explicitly selected another worktree.

## Approval

The owner approved this record on 2026-10-14 **for provisional implementation**: C1–C9 in PR #139 may be built against it, and it is ratified when #139 merges. No separate decision-only merge is required.

Implementation may show that a rule needs to change. The change is then made to this record in the same PR, called out in the PR, and approved by the owner before merge. Implementation never silently departs from the text.

**Amendments approved by the owner (2026-10-14):**
- **§2 (C1):** a lost root keeps its resources until old-root settlement is confirmed, then closes.
- **§§4–5 (C3):** retirement makes one attempt at watcher intent up to the cutoff, and its outcome never holds the retirement. The attempt never captures or acknowledges a generation after the cutoff.
- **§6 (C5):** the provisioning mutex serializes registrations, and the configuration is built under the registry guard, as at activation.
- **§5:** retirement declines while startup reconciliation runs, but not while a failed attempt waits out its backoff.
- **§5:** an enqueue that starts after `Closing` is durable and waits for the next activation; a stale client of a lost root does not block a verified replacement root.

**Amendment approved by the owner (2026-10-15):**
- **§§2, 5:** `Retired` requires the watcher to be released, not its native teardown finished. A released watcher has no authority (its callbacks are inert once the release returns); teardown runs on tracked reapers. The degraded mode's service level is ruled separately (below).
- **§5 (same day, after review):** the bound is a budget of native-watcher slots held from creation until teardown returns, at most 68 per daemon (64 active checkouts plus 4), replacing "at most 4 stranded", which counting only released watchers could not enforce: up to 68 can now be stranded if every teardown hangs. A release waits for a callback already in flight.
- **§5 degraded mode (2026-10-15):** a checkout without a native watcher (registration failure or a full budget) is a scoped exception: degraded health, the 60 s periodic full inventory, convergence and rebuild parity reported separately, no latency claim. Stage 3 counts only healthy native watching; a series during which watching degrades is invalid and restarted, never trimmed.

## Consequences

- **What goes away:** the polling ticks, lane mutexes, lock-probe priority checks, the registry's `try_lock` release, serve's replacement wait, the CLI's request polling, and the relay's serial queue.
- **Retirement can't starve, and can't revive a closed runtime.**
- **Serve answers without waiting** for index work.
- **MCP concurrency** is real, and agents get a strict, deadline-bounded decision.
- **Tests** await events and worker pause points instead of sampling lock state; functional tests assert no speed.
- **Checklist:** #138's inventory is ticked off item by item in #139.
