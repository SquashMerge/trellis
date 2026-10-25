# First Rust daemon validation

> Historical validation/analysis. Saved views, notes and Jev/ACP providers described below were retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). Their old files remain in place and ignored. Historical observations are not current feature or startup instructions.

**2026-09-18 · local macOS run · Rust/Cargo 1.98.0.**
This validates the first native indexing/storage/API slice, not the full product or
sequence-diagram usefulness. [Machine-readable record](daemon-validation.json).

## Checks

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --locked --all-targets -- -D warnings`: clean.
- `cargo test --locked --all-targets`: **37 integration tests passed**.
- `cargo build --locked`: passed.
- `node --check web/app.js`: passed.
- `npm --prefix tools/selection test` (tooling relocated after this historical run): **39 offline tests passed**.
- GitHub Actions definition added for Linux/macOS; remote CI has not run. This folder
  was not a Git repository, so no commit or push was made.

Tests cover native syntax/SCIP joins, UTF-16-to-byte coordinate matching, repeated call
identity, callbacks, computed keys, instance initializer ownership, conservative stale
resolution, parse recovery, discovery exclusions, cancellation, bounds and source hashes.

Storage checks cover CAS writers, actual foreign keys, migration, schema refusal,
reopen, durable orphans and monotonic revisions across cache loss. An injected SQLite
trigger aborts **after a first call row has been inserted**, proving that replacement
writes roll back to the preceding graph. The durable revision clock consumes a gap as
intended. The cancellation test signals during a held writer transaction; it does not
prove a specific inserted-row count. The WAL pinning test uses a raw SQLite reader.
These are not power-loss or hostile-input fuzz tests.

A regression indexes and publishes real parser output, renames a function at the same
position, and verifies that its old note becomes an orphan rather than attaching to the
new function. It also verifies numeric source ordering before query truncation and exact
normalized graph equality after persistence.

## Real feature-factory snapshot

| Measurement | Result |
| --- | ---: |
| Source files | 18 |
| Symbols | 377 |
| Measured call sites | 2,203 |
| Internal targets | 814 |
| External targets | 816 |
| Unresolved sites | 260 |
| Ambiguous sites | 313 |
| Control regions | 1,728 |
| Parse-error files / diagnostics | 0 / 0 |

All **2,203** call sites match the JavaScript spike by file, callee text and source
position after converting its UTF-16 offsets to native UTF-8 byte offsets. The native
model adds five class nodes. Nine `RepositoryConfigError` constructor sites consequently
resolve internally rather than staying unresolved, explaining the increase from 805 to
814 internal targets. This is not a general resolution precision/recall measurement.

The supplied SCIP/source manifest matches. Every recorded source/config hash remained
unchanged. Only the existing snapshot was indexed; no source scripts or indexer
subprocesses were executed, and no original repository was edited.

Thirty local HTTP requests for `transition` at depth 1 returned two symbols and five call
sites. **Debug-build median: 1.82 ms; observed range: 1.60–3.62 ms.** This includes local
HTTP/JSON overhead. It is a small warm-workspace observation, not a production latency
SLO or a whole-repository scaling benchmark.

## Browser and live API

The embedded inspector was exercised at 1440×1000 and 390×844 with no page-width overflow.
Verified token entry, exact source highlighting, default-collapsed evidence, saved view
creation/opening, annotation creation, explicit reindex/progress, reload and disconnect.
The saved view and note survived publication of revision 2 and a full daemon restart.
An old revision-1 source request returned **409** rather than new bytes for old spans.

Final browser console: no errors. Token input clears after connection; browser local and
session storage stay empty. Disconnect clears rendered/cached source and authentication.
The initial missing-favicon 404 was fixed. A restart test first signalled the tool's shell
wrapper instead of the daemon; it was corrected to signal the owned child, and graceful
SIGINT/SIGTERM behavior was then checked. No port conflict was hidden as success.

Live rejection checks: no bearer **401**, foreign Origin **403**, foreign Host **403**,
source traversal **400**, unindexed `.env` **404**, direct token URL **404**. Unit tests
also cover token permissions, symlinks, duplicate header guards, oversized bodies and
snapshot revision checks. Cache/state permissions were hardened after independent review.

[Desktop inspection screenshot](images/daemon-desktop.png).
This is deliberately an inspection client, not the final diagram canvas.

## Limits and next step

No paid inference was made; the previous experiment ledger remains closed. No Java,
watching, incremental invalidation, ACP/Jev daemon integration, PTY/Tauri, FTS or true
sequence renderer is claimed. Sources with malicious concurrent ancestor-directory
replacement are outside the current filesystem threat model.

Next: define the structured question/view-planning boundary on top of this evidence API,
then connect Jev with explicit scope and optional ACP review. Keep question relevance,
input delivery and diagram usefulness as separate validation concerns.
