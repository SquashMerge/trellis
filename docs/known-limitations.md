# Known limitations

What Trellis 0.1 doesn't do yet. If you're new to Trellis, start with the [quickstart](quickstart.md).

## Performance

Trellis misses its time targets. This doesn't block 0.1, but expect a large repository to take minutes, not seconds.

These are the numbers for a full native index plus a semantic import, on synthetic four-language repositories, on an Apple M5 Pro with 64 GB of memory (measured 2026-10-31). They don't include the time to run the producers themselves.

| Repository | Total time | Target | Native index / import | Peak memory | Index size |
| --- | ---: | ---: | --- | ---: | ---: |
| 100 files, 1.6 MB | **5.4 s** | 2 s | 2.1 s / 3.3 s | 625 MB | 0.12 GB |
| 1,000 files, 16 MB | **56 s** | 30 s | 20 s / 36 s | 1.23 GB | 1.24 GB |
| 10,000 files, 128 MB | **5 min 10 s** | 3 min | 102 s / 208 s | 2.30 GB | **6.45 GB** |

- **Time** misses the target at all three sizes.
- **Index size** misses the 4 GiB target at the largest size.
- **Memory** is within its limits at the two larger sizes. At the smallest, the 625 MB is resident memory; physical footprint is about 485 MB, within the 512 MiB limit.

**What's planned.** Time comes first, then memory, then storage. Profiling found that most of the import and much of the native index runs on one thread, and that a large share of the time goes on per-row database work. It also found two specific defects: database statements that are prepared again on every use, and a search index that's flushed after nearly every insert. The first cheap fixes could save about 70–80 seconds at the largest size. Reaching the 3-minute target needs nearly all of the proposed work, and the fixes aren't scheduled yet. The 2-second target for small repositories probably can't be met, because the native index alone takes about 2 seconds.

## Evidence

### Rust macros

Trellis doesn't expand Rust macros. It reads inside this fixed list and measures the calls in their arguments:

`assert`, `assert_eq`, `assert_ne`, `debug_assert`, `debug_assert_eq`, `debug_assert_ne`, `dbg`, `eprint`, `eprintln`, `format`, `format_args`, `panic`, `print`, `println`, `todo`, `unreachable`, `vec`, `write`, `writeln`, plus `anyhow`, `bail` and `ensure` from `anyhow`.

Everything else stays opaque, including `json!`, `matches!`, logging macros such as `tracing::info!`, your own macros, macro definitions, and attributes.

A call inside an opaque macro isn't measured, even with `rust-analyzer` imported. It doesn't appear in Sequence, in call results or in callers. Sequence shows an `Opaque macro/unsafe/const boundary`, and the file counts as [partial](quickstart.md#partial).

### Class relationships

Classes shows declarations, fields and methods. It draws no inheritance or implementation edges, in any language, and says `No proven class relationships`. `extends`, `implements` and derives are kept only as unverified syntax, and type names stay as hints.

### Syntax only until imported

Every language has syntax evidence only until you [import semantic evidence](quickstart.md#5-add-semantic-evidence). For Java and Python this matters most, because many of their calls go through objects whose type syntax alone can't determine. A call without a verified target stays labelled and isn't followed.

`scip-typescript` covers JavaScript only. Trellis doesn't index `.ts`, `.tsx` or `.jsx` files.

### Stale evidence after edits

Semantic evidence is tied to one version of the checkout. After any edit to an indexed file or project config, even one in another language, every language's evidence is [possibly stale](quickstart.md#possibly-stale) until you re-import. Sequence falls back to syntax, and possibly stale calls aren't followed. Trellis never re-runs a producer by itself. With `producer pickup`, it only re-imports a file that has changed.

### Sequence diagrams are not traces

Sequence shows static possible paths from the source, not what happened in a run. A path on the diagram may never execute, and a dynamic call may have targets that it can't show.

To keep diagrams readable, some calls are hidden as *incidental*:

- **External calls:** calls resolved into the standard library or another package.
- **Logging:** calls that look like logging by their name, such as `logger.info(...)`, `console.log(...)` and Rust `info!`. This is a naming rule, not proof that the call has no effects, and it also hides your own wrapper with such a name.
- **Trivial accessors:** calls with a verified target whose body only returns or sets a field. These need semantic evidence.

A call that may return early always stays visible. **Show N incidental calls** reveals what's hidden. A single call can count as several steps there, because its arguments are hidden with it.

Diagrams are also bounded: long methods stop at a step limit, with a warning that says so.

## Platforms and setup

### Supported platforms

- ARM64 macOS 14 or later, with Metal for local search.
- x86-64 and ARM64 Linux with glibc 2.31 or later. CPU only.

There's no build for Intel macOS, Windows or musl Linux. Binaries aren't signed or notarized.

`trellis producer run --producer rust-analyzer` works only on ARM64 macOS for now, because Trellis pins the hash of that platform's binary. Elsewhere it reports `executable sha256 … != pinned …`. On those platforms, run `rust-analyzer` 1.98.1 yourself and use [`producer pickup`](quickstart.md#feature-gated-rust-code).

### Local and single-user

Trellis runs for one user on one machine. The web app listens on loopback only and refuses other addresses, and the daemon is per user. It isn't a shared server, and it isn't built for many simultaneous clients.

### Producer versions

Trellis accepts only one exact version of each [producer](quickstart.md#install-the-producers), and it doesn't install them. `scip-java` is checked by file hash, so a launcher built another way can be refused even at the right version.

## Not in 0.1

- Connecting to remote machines over SSH.
- The Herdr terminal integration (running agents in panes inside Trellis).
- Design handoff.
