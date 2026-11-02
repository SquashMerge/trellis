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

## Install, list and remove the model

Trellis does not bundle weights. Only `trellis model install` may fetch them; search,
startup, status and listing never download them. No separate Hugging Face CLI is needed:

```sh
trellis model install
trellis model list
```

Install defaults to the selected reviewed model. You can also name the pin explicitly:

```sh
trellis model install voyageai/voyage-4-nano --revision 5fb34e870f49f19e79c94d5e118f5ee730133279
```

Only `voyageai/voyage-4-nano` at revision
`5fb34e870f49f19e79c94d5e118f5ee730133279` is supported for installation and loading.
Install downloads these nine files from that immutable revision, verifies every pinned
SHA-256 hash, and publishes the new snapshot atomically only after all files pass:

```text
1_Pooling/config.json
config.json
config_sentence_transformers.json
model.safetensors
modeling_qwen3_bidirectional.py
modules.json
sentence_bert_config.json
tokenizer.json
tokenizer_config.json
```

`modeling_qwen3_bidirectional.py` identifies the reproduced architecture but is never
executed. The authoritative descriptor is in
[`src/search_embed/mod.rs`](../src/search_embed/mod.rs).
An existing valid snapshot is fully verified without fetching. Install refuses an
incomplete or invalid existing snapshot without overwriting or repairing it. To replace it,
remove that revision explicitly, then install again:

```sh
trellis model remove voyageai/voyage-4-nano --revision 5fb34e870f49f19e79c94d5e118f5ee730133279
trellis model install
```

Remove requires both the name and `--revision`. It can remove the selected/default snapshot
or an older managed revision; listing an old revision does not make it loadable or
installable. Removal does not change configuration.

All three commands print machine-readable JSON. Install reports `verified`,
`alreadyInstalled` and `restartRequired`; remove reports `removed` and `restartRequired`.
`model list` reports `configuration`, `installed` and `appliesTo:"next_daemon_start"`.
Configuration includes the resolved paths, model pin, backend setting and per-field sources.
Listed entries include `filesPresent` for the reviewed revision (`null` for older revisions)
and `verified:false`: listing checks file presence, not hashes or loadability. Listing does
not write state, fetch files, load weights or start/contact a daemon. Its configuration is
resolved for the calling process's next daemon startup, not read from a running daemon.

## Shared model directory and optional user configuration

Snapshots use `<modelsDir>/<full-model-name>/<revision>`, including the full namespace:
`<modelsDir>/voyageai/voyage-4-nano/5fb34e870f49f19e79c94d5e118f5ee730133279`.
This is shared user-wide across checkouts, not stored in a checkout. The default model
directory is:

- macOS: `~/Library/Application Support/dev.squashmerge.trellis/models`
- Linux: `$XDG_DATA_HOME/trellis/models`, or `~/.local/share/trellis/models` when unset.

The default pin above and `backend = "auto"` work without a configuration file. Trellis
never writes defaults. Optional overrides are read from the literal path
`~/.config/trellis/config.toml`, including on macOS (not from an XDG config override):

```toml
models_dir = "/absolute/path/to/shared/models"
backend = "auto"

[model]
name = "voyageai/voyage-4-nano"
revision = "5fb34e870f49f19e79c94d5e118f5ee730133279"
```

Omit any root field to use its default. If present, `[model]` requires both fields and must
select the reviewed pin. `models_dir` must be absolute: `~` and environment variables are
not expanded in TOML. `backend` accepts `auto`, `cpu`, or `metal`. Invalid TOML, unknown
fields, unsupported selections, invalid paths/backend values and unreadable configuration
fail clearly; they do not silently fall back, even if a development override is set.

Persistent configuration and the shared model directory mean Claude Desktop's MCP launcher
does not need brittle model environment variables. Install once, optionally edit this file,
then start the daemon through the normal CLI/MCP launcher.

`TRELLIS_SEARCH_MODEL` and `TRELLIS_SEARCH_BACKEND` remain test/development overrides,
not the normal Claude Desktop setup. Resolution is field-wise: environment overrides file,
which overrides defaults. The model variable selects an absolute snapshot path instead of
the managed selection; an explicitly empty value disables the model. An unset model
variable uses the managed selection. The backend variable accepts the same three settings.

Automatic backend selection uses Metal on macOS builds with the `metal` feature, CPU
elsewhere, including Linux/OrbStack. An explicit unavailable Metal request is a typed error,
never a silent CPU fallback. Backend selection is separate from model availability;
`searchIndex.backend` reports the selected backend even while the model is unavailable.
An invalid backend setting or an unavailable explicit Metal backend refuses direct daemon
startup with the typed embedding/configuration error. Auto-start clients surface a failed
daemon startup as `daemon_unavailable`.

## Configuration and daemon restart

Configuration is daemon-wide and captured once at startup, not per checkout or MCP call.
The first CLI/MCP process that starts the daemon supplies any development environment
overrides. Later callers do not reconfigure it. Install/remove never write configuration,
select a different model, contact a daemon or restart it. Configuration edits, model
installation/removal and replacing the executable do not reconfigure an existing daemon.
A running daemon may still hold loaded weights after removal; removal is not immediate
unloading. Changes apply on the next daemon startup.

There is no `trellis daemon stop` or `trellis daemon restart` command. To apply changes,
perform these steps yourself; the model commands do not run them:

1. Stop active Trellis MCP/CLI clients so they cannot auto-start the old configuration.
2. Find the per-user daemon with `pgrep -fl '[t]rellis daemon'`, inspect its PID, and stop
   that process with `kill -TERM <pid>`. Wait for it to exit. If you started `trellis daemon`
   in a foreground terminal, Ctrl-C stops that process. Ctrl-C on `trellis serve` stops
   the serve client, not necessarily the shared daemon.
3. Run `trellis status --workspace /absolute/path/to/checkout` to auto-start the new daemon
   with the user configuration, then reconnect your clients. Remove unintended development
   overrides from that startup environment first.

Do not delete the socket or lock file to restart it; the newly elected daemon handles
any stale socket left by the stopped process.

## Configuration diagnostics, availability and lexical fallback

CLI status, `trellis_workspace_describe` and browser diagnostics expose
`searchIndex: {backend, unavailableReason, configuration}`. The daemon's immutable
`configuration` snapshot records `configFile`, `modelsDir`, `modelName`, `modelRevision`,
`modelPath`, the requested `backend`, and `sources` for `modelsDir`, `model` and `backend`.
Each source is `default`, `config_file` or `environment`. The top-level diagnostic `backend`
is the selected backend; `configuration.backend` is the requested setting, such as `auto`.
A managed `modelPath` records where startup looked even when files were missing.

`searchIndex.unavailableReason` in diagnostics and search results means:

- `model_not_configured`: required managed files were missing at startup, or the model
  environment override was explicitly empty. With default settings and no installation,
  Trellis stays lexical-only; it never tries to fetch weights.
- `model_load_failed`: the selected snapshot could not be verified or loaded. This includes
  missing files at an explicit nonempty `TRELLIS_SEARCH_MODEL` path, or a managed snapshot
  whose files were all present but failed the loader's hash/content checks. File presence
  never substitutes for the verified loader.
- `null`: no model configuration/load failure is known. Temporary loading or a missing
  matching resident matrix alone still uses `null`; this does not promise vector readiness.

Search results also retain `vector_budget_exceeded`, the existing query vector-budget
fallback reason. This query-specific reason is not part of describe/status diagnostics.

Configuration diagnostics do not report search readiness, coverage or process RSS.
Search results carry the separate query metadata: an unavailable model yields
`state:"unavailable"`, null model identity and `embedded:0`, while preserving the selected
backend and exact lexical results. Fix invalid settings or remove/reinstall invalid files,
then restart the daemon as above. A configured model loads in the background; readiness
and vector coverage may take time after startup. Neither `filesPresent`, a configured pin,
nor a null diagnostic reason proves that a query can use vectors.

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

The existing 900-second (15-minute) superseded-revision retention grace
and its head-rooted holds remain separate from the model/matrix idle timers; see
[revision retention](local-topology.md#t06--rebuild-and-cleanup).

Exact descriptor values and cache accounting are documented alongside their implementation.
The chosen saved-card CPU/Metal parity remains the runtime evidence; a changed production
card serializer needs its own retrieval evaluation before fusion constants are fixed.
