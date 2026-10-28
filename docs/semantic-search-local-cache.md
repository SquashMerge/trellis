# Local semantic-search model and vector cache

Semantic symbol search combines lexical retrieval with optional local embeddings. The daemon
loads a pinned voyage-4-nano snapshot and shares a derived vector cache across checkouts.
Lexical retrieval remains available without a configured or ready model.

## Release and source builds

Official macOS ARM64 releases include Metal; Linux releases are CPU-only. The release
helper selects these features explicitly (see [Release builds](releases.md)). Ordinary
Cargo builds still default to CPU-only; use `cargo install --path . --locked --features metal`
for a Metal-enabled macOS source install. Automatic backend selection may fall back to
CPU; explicitly selecting `TRELLIS_SEARCH_BACKEND=metal` never silently falls back.

## Install the model

Trellis does not bundle weights, download a model or execute upstream Python. Acquire the
supported snapshot explicitly with Hugging Face's `hf` CLI (installed separately). Use an
absolute directory outside your checkouts; the following location is a recommendation,
not a directory Trellis discovers automatically:

```sh
export TRELLIS_SEARCH_MODEL="$HOME/.local/share/trellis/models/voyage-4-nano/5fb34e870f49f19e79c94d5e118f5ee730133279"
hf download voyageai/voyage-4-nano \
  --revision 5fb34e870f49f19e79c94d5e118f5ee730133279 \
  --local-dir "$TRELLIS_SEARCH_MODEL" \
  1_Pooling/config.json config.json config_sentence_transformers.json \
  model.safetensors modeling_qwen3_bidirectional.py modules.json \
  sentence_bert_config.json tokenizer.json tokenizer_config.json
```

These are all nine required files. Trellis verifies their pinned SHA-256 hashes before
loading, including `modeling_qwen3_bidirectional.py`, which identifies the reproduced
architecture but is never executed. The authoritative descriptor is in
[`src/search_embed/mod.rs`](../src/search_embed/mod.rs). Other revisions or incomplete
snapshots are not supported.

For Metal acceleration on macOS, install from the repository with the explicit feature:

```sh
cargo install --path . --locked --features metal
```

The default build has no `metal` feature. This does not change release defaults. CPU-only
installation remains `cargo install --path . --locked`.

## Configuration and daemon restart

Set these environment variables before starting the per-user daemon, including a CLI or
MCP client that may start it automatically:

- `TRELLIS_SEARCH_MODEL`: absolute path to the downloaded snapshot directory. Unset means no model.
- `TRELLIS_SEARCH_BACKEND`: `auto` (default), `cpu`, or `metal`.

```sh
export TRELLIS_SEARCH_MODEL="$HOME/.local/share/trellis/models/voyage-4-nano/5fb34e870f49f19e79c94d5e118f5ee730133279"
export TRELLIS_SEARCH_BACKEND=auto
trellis status --workspace /absolute/path/to/checkout
```

Configuration is daemon-wide, not per checkout or MCP call. There is no persistent model
configuration file. Put these exports in your shell startup file or in the environment of
your editor/agent's MCP launcher. A GUI-launched editor may not read your shell startup file.
The daemon inherits the environment of the first CLI or MCP process that starts it; later
callers cannot change a running daemon's configuration. Reinstalling the executable also
does not replace a running daemon.

There is no `trellis daemon stop` or `trellis daemon restart` command. To apply changed
settings or a new binary:

1. Stop active Trellis MCP/CLI clients so they cannot auto-start the old configuration.
2. Find the per-user daemon with `pgrep -fl '[t]rellis daemon'`, inspect its PID, and stop
   that process with `kill -TERM <pid>`. Wait for it to exit. If you started `trellis daemon`
   in a foreground terminal, Ctrl-C stops that process. Ctrl-C on `trellis serve` stops
   the serve client, not necessarily the shared daemon.
3. Export the settings and run `trellis status --workspace /absolute/path/to/checkout`
   from that environment to auto-start the new daemon, then reconnect your clients.

Do not delete the socket or lock file to restart it; the newly elected daemon handles
any stale socket left by the stopped process.

Automatic backend selection uses Metal on macOS builds with the `metal` feature, CPU
elsewhere, including Linux/OrbStack. An explicit unavailable Metal request is a typed error,
never a silent CPU fallback. Backend selection is separate from model availability;
`searchIndex.backend` reports the selected backend even while the model is unavailable.
An invalid backend setting or an unavailable explicit Metal backend refuses direct daemon
startup with the typed embedding/configuration error. Auto-start clients surface a failed
daemon startup as `daemon_unavailable`.

## Availability and lexical fallback

`trellis_search_symbols`, `trellis_workspace_describe` and `trellis status` report
`searchIndex.unavailableReason`:

- `model_not_configured`: `TRELLIS_SEARCH_MODEL` was unset when the daemon started.
- `model_load_failed`: the configured snapshot could not be verified or loaded, for example
  because files are missing or do not match the pinned hashes.
- `null`: no model configuration/load failure is known. Temporary model loading or a missing
  matching resident matrix alone still uses `null`; this does not promise vector readiness.

Search results also retain `vector_budget_exceeded`, the existing query vector-budget
fallback reason. This query-specific reason is not part of describe/status diagnostics.

Workspace describe and CLI status expose only the diagnostic pair
`searchIndex: {backend, unavailableReason}`; they do not report a search pin, coverage counts
or readiness state. A null reason can mean untried or loading, not ready. Search results
include the full search metadata: an unavailable model yields `state:"unavailable"`, null
model identity and `embedded:0`, while preserving the selected backend and exact lexical
results. Fix the model directory or startup environment and restart the daemon as above.
A configured model loads in the background; readiness and vector coverage may take time
after startup.

The [resident-model and matrix amendments](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md#model-identity-and-runtime)
supersede the earlier per-operation model lifetime. The single embedding/fill owner
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
evicting matrices for another startup checkout. Search result state remains unavailable while
the model is not ready. CLI status/GC do not fabricate search coverage.

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
that bypasses the cap. When only matrix availability prevents vector retrieval, the
unavailable status keeps null model identity, embedded:0, selected backend and
unavailableReason:null. Mandatory declaration counts and lexical
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
