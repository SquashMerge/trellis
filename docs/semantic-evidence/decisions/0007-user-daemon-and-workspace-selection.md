# Decision 0007: user-level daemon, per-call workspace selection and idle timeouts

- **Status:** owner-approved 2026-10-06; ratified when this record merges. It governs #107, and #17 builds on it. It amends `../../mcp-readonly-pilot-contract.md` (#24) where noted, and adds to `../../local-topology.md`.
- **Scope:** how Trellis processes are arranged when many coding-agent sessions run at once, how an MCP call chooses its workspace, and when idle resources are released. Per-checkout indexes, pins, revisions, retention (Decision 0006), the #67 queue and the evidence contracts are unchanged.
- **Compatibility:** none needed (pre-release).

## Background

Agent clients start a stdio MCP server per session, and subagents share their parent session's server. With many sessions open across many checkouts, a process per session costs memory and contends for each checkout's leader and watcher (#16). `trellis mcp` also binds one workspace at startup, so a subagent working in its own git worktree gets evidence for the parent's checkout.

## Decision

### 1. One daemon per user

- A single Trellis daemon per user serves every checkout and worktree, enforced by a single-instance lock under Trellis's private per-user directory.
- **Start:** on demand when a client attaches and none is running.
- **Crash recovery:** clients reconnect, or restart it.
- **Per checkout, as before:** the daemon competes for each active checkout's leader lock like any other Trellis process. If a standalone CLI already holds it, the daemon stays a follower for that checkout and never forces a takeover. As leader, it runs that checkout's watcher and reconciliation (#16), request queue (#67), retention and GC (Decision 0006). Indexes, pins and revisions stay **per checkout** and are never merged across branches or worktrees. Reuse of extraction work across worktrees comes from #71's path-neutral fact cache.
- **`trellis mcp` becomes a thin client.** It speaks MCP over stdio to the agent client exactly as #24 specifies, and relays to the daemon over a Unix domain socket in an owner-only directory. Access is protected by file-system permissions (T00).
  - It still exits on stdin end-of-file and still never indexes.
  - "Never daemonizes" applies to the client. The daemon is a separate process the client may start.
- **CLI commands** use the daemon when it's running. Without it, they keep working standalone under the existing per-checkout leader lock.
- **The browser** is served by the daemon on one loopback port with the existing token auth.
  - The page lists the checkouts the daemon knows: active ones, plus any with an existing index.
  - Every checkout-scoped browser API request names its checkout (for example in the URL path); global listing, health and daemon-status requests name none. There is no implicit default when more than one checkout exists.

### 2. Per-call workspace selection (amends #24)

- A call without a selection is answered for the client's **launch workspace**: `--workspace`, or the nearest Git checkout from the client's launch directory, as today.
- An evidence tool call may **explicitly** select another workspace, but only a **worktree of the same repository** as the launch workspace (the same git common directory).
- **Verification:** the daemon verifies the selected path's root identity under the #23 rules before answering. It never falls back to another worktree. A path outside the repository, or one it can't verify, is refused with a typed error.
- **Attribution:** every result reports which workspace answered.
- **Attachment:** the first explicit selection of a worktree attaches it to the client's session, and it stays attached while that client is connected. A session may have several worktrees attached.
- **Contract change:** this replaces #24's rule that "no tool argument selects another workspace". Evidence from two worktrees still never mixes in one result, and nothing is selected implicitly. #107 adds the optional workspace field to the tools' closed input schemas, and the matching result field, with tests. #17's criteria are updated to match.

### 3. Idle timeouts

Both are policy constants, to be tuned later without a format change.
- **Checkout release: 15 minutes** after the last client attached to that checkout disconnects, and only with no queued or in-flight work for it.
  - A checkout's clients are the sessions that launched in it or explicitly selected it (§2).
  - An open browser counts as a client of its selected checkout while it is **active**, meaning it made a request within the last 15 minutes.
  - While any client is attached, the checkout's watcher keeps running, so evidence stays live for open sessions.
  - On release, the daemon closes the watcher and connections, and releases the leader lock and the retained SQLite check handles (see #16).
  - The next client re-attaches it with a catch-up scan and publication.
- **Daemon exit: 30 minutes** after the last client of any checkout disconnects, counting active browsers as above, and only with no queued or in-flight work anywhere.

**Owner ruling, 2026-10-11 (#130):** “Anywhere” means checkouts activated by this daemon. Idle exit does not scan other index directories or their orphan queues. Unknown entries, unreadable queues and accepted rows in never-activated checkouts cannot veto exit. Those rows remain durable and recover on the next activation (Decision 0008 §3). The 15-minute release and 30-minute exit constants are unchanged.

## Consequences

- **Fewer processes:** N agent sessions cost one daemon plus N small clients instead of N full servers.
- **Correct worktree evidence:** a subagent can get evidence for its own worktree by selecting it explicitly.
- **More concurrency inside one process, not more processes.** #16's per-checkout correctness rules apply unchanged inside the daemon.
- **Single point of failure:** a daemon crash affects every session until clients reconnect. Accepted requests survive in each checkout's queue.

## Implementation impact (#107)

- **Daemon process:** single-instance lock, socket, start-on-attach, idle exit, crash recovery.
- **Thin `trellis mcp` client:** relays over the socket and handles launch-workspace defaulting.
- **Checkout lifecycle:** attach and release per §3, with watcher and handle cleanup.
- **Tools:** the optional `workspace` input on evidence tools, and workspace attribution in results, per §2.
- **Process boundaries:** the CLI attaches when the daemon is running and stays standalone otherwise, and the daemon serves the browser.
- **Contract:** update `mcp-readonly-pilot-contract.md`'s closed schemas and examples alongside the implementation.
