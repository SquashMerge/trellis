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

Trellis publishes native results first and catches semantic evidence up in the background ([native-first publication, #668](https://github.com/SquashMerge/trellis/pull/668)). Reads stay available, and responses label older-basis, catching-up and needs-re-import evidence until catch-up finishes. Catch-up time is **not bounded**. A producer's own re-run, which you start or which [automatic re-runs](#automatic-re-runs) start, is a separate and much longer step.

In a 2026-11-05 dogfood run on a copy of the Trellis checkout with Rust and JavaScript imported, native results were readable in mostly 1–3 s. Median semantic catch-up was about 32 s for the first edit after daemon start, 9–13 s for later edits, and about 113 s for a `Cargo.toml` edit on a fresh daemon, with peak RSS of 3.6–7.4 GiB. The host load was not controlled and single runs varied up to 2×. These are observations from a loaded host, not a controlled benchmark or a latency guarantee. See the [0.1.0 release notes](release-notes-0.1.md#catch-up-performance).

An eligible `producer run` import uses witnessed catch-up only while its inputs and witness eligibility hold. Multi-artifact batches; changed producer, source set, toolchain, dependencies or capture options; and a missing or released original import still take a full semantic rejoin.

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

Trellis doesn't index `.jsx`.

### Semantic evidence after an upgrade

An index rebuild does not make retained producer evidence fresh by itself. Trellis tries the last successful `producer run` batch again through normal validated import. Missing inputs, a changed native capture or incompatible producer pins leave a typed **semantic evidence needs re-import** reason in status, the web app and MCP. Trellis runs the producer by itself only if you opted in to [automatic re-runs](#automatic-re-runs).

The first upgrade onto this behavior needs re-import because previous versions deleted successful run inputs. This applies only to languages a producer covered (a successful `producer run` or a configured pickup); a language that never had a producer stays not configured. There is no old-index migration. Only one successful run's inputs are retained per producer, in place, and GC may remove them. This is not an artifact archive. See [upgrade recovery](quickstart.md#upgrading-trellis).

### Stale evidence after edits

Semantic evidence is tied to one version of the checkout. After any edit to an indexed file or project config, the changed file's evidence is stale, and every other file's evidence, in every language, is [possibly stale](quickstart.md#stale-and-possibly-stale) until you re-import. Sequence falls back to syntax, and possibly stale calls aren't followed. Trellis re-runs a producer by itself only if you opted in to [automatic re-runs](#automatic-re-runs). With `producer pickup`, it only re-imports a file that has changed.

### Automatic re-runs

Automatic producer re-runs ([quickstart](quickstart.md#automatic-re-runs-optional)) have these limits:

- **Off by default.** Nothing starts until you enable a producer in `.trellis/producers.toml` and one manual run of it has imported. A missing, changed-toolchain or replaced-checkout approval needs a new manual run.
- **A run is expensive.** It is the same run a manual `producer run` makes: a build or analysis of the whole checkout, with the producer's own CPU, memory and disk use, and one more published revision per import.
- **A 60-second floor and two gates.** `minIntervalSecs` cannot be set below 60, and a start is skipped on battery or at a load at or above the CPU count. Where the power state or load can't be read, the gate doesn't apply. The gates are checked at start only.
- **Manifest-only changes don't trigger.** A change that touches only a manifest, with no indexed source document added, changed or removed in a language the producer covers, doesn't make a run due.
- **Manual overrides aren't used.** Automatic runs re-derive settings from your project's files; options such as `--cargo-features` that you passed to a manual run are not stored in the approval.
- **The rate limit and the failure back-off survive a restart.** They are restored from the producer's newest automatic run record, and lapse only if count retention has pruned it.
- **Some failures are remembered only for the checkout runtime's lifetime, until the checkout is reactivated.** A failure that happens before a run record is written (a refused plan preparation, an error before the run starts, a thread that could not start) is not saved. Each time the checkout is reactivated, Trellis tries it once more at an unchanged head; the attempt is cheap and starts no producer.
- **Browser views show committed labels only.** The web app doesn't show the scheduled or running **catching up** label; `trellis status`, `producer status` and MCP do.

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

### TypeScript configs shared through npm

A producer run's worktree holds no `node_modules`. When the root `tsconfig.json` `extends` an npm package, such as `@sindresorhus/tsconfig`, Trellis reads that package's config from the checkout's own `node_modules` and supplies it to the run ([#799](https://github.com/SquashMerge/trellis/issues/799)). Dependencies must be installed for this; without them, the run uses Trellis's generated tsconfig in place of the repo's, so the repo's own compiler options don't apply. The plan lists the replacement under `omitted`. Types from other installed packages are still not visible to the run. See [producer provisioning](semantic-evidence/producer-provisioning.md#scip-typescript-project-config).

## Not in 0.1

- Connecting to remote machines over SSH.
- The Herdr terminal integration (running agents in panes inside Trellis).
- Design handoff.
- C4 and Pele class parity.
- Drawn class edges ([#512](https://github.com/SquashMerge/trellis/issues/512)).
- CI speed work ([#495](https://github.com/SquashMerge/trellis/issues/495)).

These exclusions follow [the 0.1 scope (#529)](https://github.com/SquashMerge/trellis/issues/529); they are not promises for the next release.
