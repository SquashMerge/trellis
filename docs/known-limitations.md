# Known limitations

What Trellis 0.1 doesn't do yet. If you're new to Trellis, start with the [quickstart](quickstart.md).

## Performance

The combined native-index and semantic-import [performance gate (#61)](https://github.com/SquashMerge/trellis/issues/61) remains open. [The 0.1 scope](https://github.com/SquashMerge/trellis/issues/529) treats it as a known limitation, not a release blocker. Large repositories can take minutes to index and import.

These are **historical measurements**, not a benchmark of the current release candidate: the [corrected 2026-10-31 campaign](https://github.com/SquashMerge/trellis/issues/61#issuecomment-6539835295), on commit `516442d0`, using synthetic four-language repositories and the Apple M5 Pro / 64 GB reference host. Times cover full native indexing, semantic import and publication, but exclude producer execution and search embedding work.

| Repository | Median total | Target | Peak daemon RSS | Steady index size |
| --- | ---: | ---: | ---: | ---: |
| 100 files, 1.6 MB | **5.39 s** | 2 s | 625 MB | 0.12 GB |
| 1,000 files, 16 MB | **56.3 s** | 30 s | 1.23 GB | 1.24 GB |
| 10,000 files, 128 MB | **309.6 s** | 180 s | 2.30 GB | **6.45 GB** |

All three missed their time targets; the largest also exceeded the 4 GiB steady-index limit. The storage-to-canonical-evidence ratio was **not measured** because its denominator was not defined. The earlier `20261031b` campaign was contaminated by search embedding work and is not valid evidence.

The [owner's macOS memory ruling](https://github.com/SquashMerge/trellis/issues/61#issuecomment-6543241975) uses lifetime peak physical footprint, not RSS, for the gate. SMALL's observed footprint was about 485 MB, within the 512 MiB limit but with little headroom. [#511](https://github.com/SquashMerge/trellis/issues/511) remains open; it also records a separate 2026-11-01 dogfood observation of 2.31–2.93 GiB idle daemon RSS after imports. That is not the synthetic gate's memory measurement or a guaranteed idle budget.

[#518's profiling](https://github.com/SquashMerge/trellis/issues/518#issuecomment-6543795684) identified serial work and per-row SQLite costs. Its proposed savings are estimates, not measured improvements or a promise that the targets will be met.

### Edits after a large semantic import

The [merged edit-path fix (#573)](https://github.com/SquashMerge/trellis/pull/573) reduced memory for ordinary edits on its tested Rust artifacts, but did not bound a full semantic rejoin. [#574 remains open](https://github.com/SquashMerge/trellis/issues/574): a `Cargo.toml` edit with a roughly 67 MB artifact was measured at about **21 GB peak memory and 756 seconds**. This was a shared-host test, not a quiet reference-host benchmark.

Config, toolchain or extractor changes, multi-artifact batches and `scip-java` can still take that full path. Ordinary edits can also take tens of seconds because publications rewrite semantic rows. The proposed lower-memory full path is not a measured fix.

## Evidence

### Rust macros

Trellis doesn't run compiler macro expansion. It parses the arguments of this fixed list and measures calls inside them:

`assert`, `assert_eq`, `assert_ne`, `debug_assert`, `debug_assert_eq`, `debug_assert_ne`, `dbg`, `eprint`, `eprintln`, `format`, `format_args`, `panic`, `print`, `println`, `todo`, `unreachable`, `vec`, `write`, `writeln`, plus `anyhow`, `bail` and `ensure` from `anyhow`.

Everything else stays opaque, including `json!`, `matches!`, logging macros such as `tracing::info!`, your own macros, macro definitions, and attributes.

A call inside an opaque macro isn't measured, even with `rust-analyzer` imported, so call results and callers omit it. Format strings that name arguments, and named-argument names, also remain gaps inside the allowlisted macros; an argument list that cannot be parsed within the bounds stays opaque. These gaps can make a file [partial](quickstart.md#partial).

Sequence still treats **every** Rust macro as a boundary, including the allowlisted macros whose calls are measured for call results. It shows an `Opaque macro/unsafe/const boundary`. Recognized logging macros instead show a logging boundary, which is [hidden by default](#sequence-diagrams-are-not-traces). See [the macro decision](semantic-evidence/decisions/0019-rust-expression-macro-measurement.md).

### Class relationships

Classes shows declarations, fields and methods. It draws no inheritance or implementation edges, in any language, and says `No proven class relationships`. `extends`, `implements` and derives are kept only as unverified syntax, and type names stay as hints. Drawing edges from proven semantic relationships is [open work (#512)](https://github.com/SquashMerge/trellis/issues/512), not a 0.1 capability.

### Syntax only until imported

Every language has syntax evidence only until you [import semantic evidence](quickstart.md#5-add-semantic-evidence). For Java and Python this matters most, because many of their calls go through objects whose type syntax alone can't determine. A call without a verified target stays labelled and isn't followed.

`scip-typescript` covers JavaScript only. Trellis doesn't index `.ts`, `.tsx` or `.jsx` files.

### Stale evidence after edits

Semantic evidence is tied to one version of the checkout. After any edit to an indexed file or project config, the changed file's evidence is stale, and every other file's evidence, in every language, is [possibly stale](quickstart.md#stale-and-possibly-stale) until you re-import. Sequence falls back to syntax, and possibly stale calls aren't followed. Trellis never re-runs a producer by itself. With `producer pickup`, it only re-imports a file that has changed.

### Sequence diagrams are not traces

Sequence shows static possible paths from the source, not what happened in a run. A path on the diagram may never execute, and a dynamic call may have targets that it can't show.

To keep diagrams readable, some calls are hidden as *incidental*:

- **External calls:** calls resolved into the standard library or another package.
- **Logging:** calls that look like logging by their name, such as `logger.info(...)`, `console.log(...)` and Rust `info!`. This is a naming rule, not proof that the call has no effects, and it also hides your own wrapper with such a name.
- **Trivial accessors:** calls with a verified target whose body only returns or sets a field. These need semantic evidence.

A call that may return early always stays visible. **Show all steps** restores admitted detail and incidental calls; **Show calls** returns to the calls-first view. This does not recover calls omitted by backend limits.

Diagrams are also bounded: long methods stop at a step limit, with a warning that says so.

## Platforms and setup

### Supported platforms

- ARM64 macOS 14 or later, with Metal for local search.
- x86-64 and ARM64 Linux with glibc 2.31 or later. CPU only.

There's no build for Intel macOS, Windows or musl Linux. Binaries aren't signed or notarized.

### Local and single-user

Trellis runs for one user on one machine. The web app listens on loopback only and refuses other addresses, and the daemon is per user. It isn't a shared server, and it isn't built for many simultaneous clients.

### Search model download

The embedding model is **not bundled**. Only an explicit `trellis model install` downloads the pinned weights; startup and search never fetch them. Without a ready model, search still returns lexical results. Installing the model does not reconfigure a running daemon, and vector coverage can take time after startup. See [local model setup and restart instructions](semantic-search-local-cache.md).

### Producer versions

Trellis accepts only one exact version of each [producer](quickstart.md#install-the-producers), and it doesn't install them. `scip-java` is checked by file hash, so a launcher built another way can be refused even at the right version.

## Not in 0.1

- Connecting to remote machines over SSH.
- The Herdr terminal integration (running agents in panes inside Trellis).
- Design handoff.
- C4 and Pele class parity.
- Drawn class edges ([#512](https://github.com/SquashMerge/trellis/issues/512)).
- CI speed work ([#495](https://github.com/SquashMerge/trellis/issues/495)).

These exclusions follow [the 0.1 scope (#529)](https://github.com/SquashMerge/trellis/issues/529); they are not promises for the next release.
