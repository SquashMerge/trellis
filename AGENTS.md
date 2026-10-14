# Working in Trellis

## How Trellis is used

Trellis is a local developer tool. One developer runs it on their own machine, across a few checkouts. A checkout has at most **2–3 Trellis processes** at once: the daemon (index leader and file watcher, which also serves the browser), plus one CLI or MCP session. It is not a server and not multi-user, and it listens on loopback only. Don't design or test for heavy concurrency or many simultaneous clients.

Trellis is pre-release, so there is **no backward compatibility**. Old or intermediate formats of **disposable derived state** (the indexes) are recreated where their contract permits it, never migrated or specially handled. Durable state, such as saved views, notes and the request queue, follows its own contract in `docs/local-topology.md`.

## Threat model: build and review to it, not beyond it

The threat model is [`docs/local-topology.md` § T00](docs/local-topology.md#t00--threat-model). It defines what every Stage 2–4 implementation and review must defend against. A review finding outside T00 is not a blocker; at most it's a note.

Nobody is attacking Trellis from inside the user's own account. A process running as the same user can already change the source, the binary and the configuration, so defenses against it add no security.

Do not add, and do not ask for:

- Signing, MACs, re-authentication or tamper detection on Trellis's own state (`index.db`, `requests.db`, the fact cache, locks, markers) against a same-user process.
- Defenses against hostile same-user replacement of filesystem ancestors (see T02).
- New per-field read-side size caps beyond the existing request and response bounds, or new re-validation of stored rows against captured source on read.

T00 removes these as *requirements*. It doesn't license removing checks that already exist, such as selected-document read attestation; changing those is an owner decision.

Do handle, as T00 requires:

- **Untrusted repository content:** bounded parsing that never crashes the process.
- **Untrusted callers** of the daemon, browser API and MCP server: authenticated where the contract says so, bounded and validated.
- **Ordinary faults:** crashes, power loss, partial writes, full disks, I/O errors, concurrent Trellis processes, and races with edits and `git checkout`.
- **Accidental corruption:** typed corrupt failures, never partial results.

If an acceptance criterion or a ratified decision explicitly requires a defense, build it, even if T00 doesn't. Otherwise, a proposed hardening that cites an attacker T00 excludes is out of scope; say so instead of building it.

## Concurrency is a correctness concern, not a security one

Races between Trellis's own processes are in scope as ordinary faults. Fix them as product defects (SQLite busy handling, ordering, reconciliation), not as adversarial input. Lessons from #67:

- **Never close a separate file handle on a live SQLite file.** On POSIX, `close()` on any descriptor drops all of the process's locks on that file, including SQLite's own.
- **Contention is typed and retried.** It becomes `storage_busy` with a bounded wait and retry. It is never a raw SQLite error, and never a failed or undone request. A terminal request row never moves backwards.
- **No panicking `eprintln!` in long-running daemon loops.** Use writes that ignore errors, so a closed stderr can't stop the loop.

### In-process coordination: owners and signals

For new or changed daemon coordination, prefer one owner for each mutable workflow. Callers submit work and observe published state; preserve the ratified H, FIFO and watcher order. Use signals for in-process completion and hooks for test ordering. A timeout is a liveness backstop, not proof of completion.

A failed `try_lock` permits conservative deferral and typed retry, but proves nothing about pending work or its owner. Recheck decisions at the owning transition; do not act on an incoherent combination of sampled flags. Do not await an actor while holding a registry or other lock that actor needs.

Clean up operation-scoped requests on every exit, including dropped futures, without clearing a successor's state. Keep periodic reconciliation and bounded external-contention retries. Explain any new polling or `try_lock`-based cross-component decision that cannot use an owner-published signal.

## Building and testing proportionately

- Build the simplest design that meets the acceptance criteria, and no mechanism beyond them.
- Trust what Trellis itself wrote and validated. Never validate data that's about to be discarded or rebuilt.
- Test each distinct required behavior, at normal size. One acceptance criterion may need several tests, for example one per failure path. Use production-size data only when a criterion requires it.
- **Concurrency tests:**
  - model at most 2–3 processes;
  - prefer deterministic, hook-synchronized tests that pause one process at the critical point;
  - use repeat loops (about 20 runs under modest CPU load) only for failures that have actually been intermittent.

  A failure inside a loop is a defect to report with its log, not something to retry until it passes.
- **Before submitting,** run `./tools/verify` and `cargo fmt --all -- --check`. Run `cargo clippy --locked --all-targets -- -D warnings` on the CI toolchain (currently Rust 1.99).

## Where decisions live

Owner rulings and contracts live in:
- [`docs/semantic-evidence/decisions/`](docs/semantic-evidence/decisions/);
- [`docs/semantic-evidence/contract-v1.md`](docs/semantic-evidence/contract-v1.md);
- [`docs/local-topology.md`](docs/local-topology.md).

Issue bodies point at these rather than restating them. When they conflict, the ratified record wins.
