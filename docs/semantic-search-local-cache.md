# Local semantic-search model and vector cache (#211)

This slice provides index-independent model/cache infrastructure. It does not capture cards,
add an FTS table, enable `trellis_search_symbols`, or change `index.db`. Those changes wait for
the `feature-scip` merge. The card format is a separate [review draft](semantic-search-card-format-draft.md).

## Configuration

Set these environment variables when starting the per-user daemon (including automatic startup):

- `TRELLIS_SEARCH_MODEL`: path to a local copy of the supported pinned model. Unset means no model.
- `TRELLIS_SEARCH_BACKEND`: `auto` (default), `cpu`, or `metal`.

Configuration is daemon-wide, not per checkout or MCP call. Restart the daemon to change it.
Trellis does not bundle weights, download a model or execute upstream Python. The supported
model descriptor verifies all nine pinned snapshot files before loading. Missing, mismatched
or loading models are unavailable for vector retrieval; lexical retrieval remains available
when the later search tool is wired. No current tool changes behavior in this slice.

Automatic selection uses Metal on macOS builds with the `metal` feature, CPU elsewhere,
including Linux/OrbStack. An explicit unavailable Metal request is a typed error, never a
silent CPU fallback. Backend selection is separate from model availability. The internal
`searchIndex` status snapshot includes the selected backend even while model state is
unavailable/loading; public emission waits for the later search-tool slice. No platform
release-build work is included here (#338).

An invalid backend setting or an unavailable explicit Metal backend refuses direct daemon
startup with the typed embedding/configuration error. Existing auto-start clients currently
surface a failed daemon startup as `daemon_unavailable`; this slice does not add a startup
error protocol or a new MCP failure code. A running daemon keeps its own startup configuration,
not the environment of a later caller.

The model is loaded only inside an operation and dropped on every exit. The status handle
reports unavailable before loading, while loading, and after dropping weights. While a model
job is active, its caller supplies one coherent declaration/vector-coverage snapshot and
whether a background fill job is running; this distinguishes complete, building and partial.
The later tool will emit that status. Existing CLI status/GC do not fabricate search coverage.

## Vector cache

The daemon owns one machine-wide `<cache>/vectors.db`, shared across checkouts. Keys combine
`modelId` and the exact card-text hash. Vectors are signed int8 with at most 1024 dimensions.
The default LRU budget is 1 GiB for the database file, not just the vector payload. No checkout publication or evidence coverage depends on
this optional derived cache. Reads do not write cache or age metadata.

A row that fails typed decoding is a cache miss. Database corruption/schema/I/O failures are
`store_unavailable` for readers, not lexical-only success or partial results. Reads trust
published rows and do not run a whole-database integrity scan. The daemon writer recreates
a corrupt or incompatible disposable cache on open; it does not migrate old formats.
I/O failures are not permission to discard the cache. Contention stays typed `storage_busy`
with bounded waiting. `trellis gc --report` reports cache bytes and entry count without
creating a database, touching LRU state or changing open age.

Exact descriptor values and cache accounting are documented alongside their implementation.
The chosen saved-card CPU/Metal parity remains the runtime evidence; a changed production
card serializer needs its own retrieval evaluation before fusion constants are fixed.
