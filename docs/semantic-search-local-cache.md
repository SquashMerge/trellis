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

The [resident-model and matrix amendments](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md#model-identity-and-runtime)
supersede this slice's original per-operation model lifetime. The single embedding/fill owner
reuses verified weights and resident int8 head matrices. Model weights expire after 600 seconds
with no query or fill work. Each matrix expires 600 seconds after its last query use or
publication; background fill does not reset its timer. A running head is excluded from idle
eviction until its job ends. Model readiness pre-warms active-checkout heads. Under the
[#358 owner policy](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6446057555),
startup pre-warms at most four checkouts opened within the last seven days, most recent
first with root-key byte-order ties. Existing index metadata is the approved open-recency
proxy; discovery uses it read-only without activating checkouts, refreshing open age or
creating a durable MRU catalog. This low-priority background work yields to normal work,
never delays daemon socket acceptance and stops at matrix-cap admission failure without
evicting matrices for another startup checkout. Status remains unavailable while the model
is not ready. Existing CLI status/GC do not fabricate search coverage.

Under the [#358 owner ruling](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6441405819),
one immutable published matrix snapshot supplies vector ranks and embedded counts for the
same admitted index pin and active model as the mandatory index snapshot. New head pins patch
only changed document-version IDs from vectors.db, and the existing fill owner tops up missing
vectors. The aggregate matrix allocation cap is 2 GiB, including in-progress builds,
replacements and staging buffers. The owner conservatively accounts for estimated allocation
capacity and bookkeeping before allocation rather than only vector payload. This is not an
exact process-RSS cap and does not include model weights. Query use shares the owner's
existing matrix charge, not a second copy. Normal work uses LRU eviction of eligible matrices. The 600-second
idle rule remains in addition to the cap. A head that alone cannot fit stays lexical-only.
A checkout not warmed at startup returns complete lexical results on first search while
the independent background owner builds its matrix. No query waits for that build or
embeds a query without an admitted resident matrix. A missing matching matrix after
eviction or a head-change race also yields lexical-only results, not a streaming scorer
that bypasses the cap. The unavailable status keeps null model identity, embedded:0,
selected backend and unavailableReason:null. Mandatory declaration counts and lexical
retrieval remain exact; observed storage faults remain typed failures. This supersedes
the earlier missing-matrix streaming fallback. Backing-cache eviction need not invalidate
an already published resident snapshot. Query fallback at the 3,000ms vector
budget or cancellation/drop does not stop the independent owner build from finishing and
publishing; query model inference is never detached. The 5,000ms call deadline is unchanged.
Scoring uses NEON/SIMD with scalar fallback and the stored-norm dot product; gold-set
R@5/R@10/MRR must not regress, with no bit-parity requirement. sqlite-vec is not adopted. Existing bounds and typed
database-fault behavior remain unchanged.

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
