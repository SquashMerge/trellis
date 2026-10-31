# Producer provisioning (#21, AC7e)

The real-producer end-to-end tests (`tests/producer_e2e*.rs`) run the four pinned producers through `trellis producer plan` and `trellis producer run`. **Trellis installs nothing.** Provisioning a producer is a host action by the owner. The tests only detect and verify what is already there, and they never run a package manager, `rustup component add`, `coursier bootstrap` or a download.

Pins live in [`hardware/mac-mini-m5-pro-v1.json`](hardware/mac-mini-m5-pro-v1.json). The producer rows record `installMethod` as it was captured. The table below records what the reference host runs today.

## Reference host

| Producer | Pin | Install method | Launcher location class | Verification | Observed runtime |
|---|---|---|---|---|---|
| scip-typescript | 0.4.0 | `npm install -g @sourcegraph/scip-typescript@0.4.0` under a mise-managed node | npm global `bin/` entry, a symlink to the package's `dist/src/main.js`, next to a native `node` | `packageManifestVersion`: `package.json` name and version of the launcher's package equal the pin | node v24.22.1 (the profile text says 24.11.1; the node binary is hashed into the run's provenance, not compared with that text) |
| scip-python | 0.6.6 | `npm install -g @sourcegraph/scip-python@0.6.6` under the same node | npm global `bin/` entry, a symlink to the package's `index.js`, next to a native `node` | `packageManifestVersion` | node v24.22.1 |
| scip-java | 0.12.3 | `coursier bootstrap --standalone com.sourcegraph:scip-java_2.13:0.12.3` | a standalone launcher file on the declared `PATH` | `executableSha256` equals the pinned `executableHash` | mise-installed Temurin 25.0.3 (`~/.local/share/mise/installs/java/temurin-25.0.3+9.0.LTS/bin`) on the declared `PATH`; the scip-java JVM is Temurin 17.0.20.1, see below |
| rust-analyzer | 1.98.1 | `rustup component add rust-analyzer` on toolchain 1.98.1 | the `rustup` proxy on the declared `PATH`; the real binary is `$RUSTUP_HOME/toolchains/1.98.1-<triple>/bin/rust-analyzer` | `executableSha256` of that binary equals the pin | `1.98.1-aarch64-apple-darwin` |

A shim launcher (mise, asdf, pyenv) or a shim `node` is refused with `producer_unavailable`: detection reads files and never executes a shim to ask its version.

The scip-java and rust-analyzer rows are confirmed by the strict runs of the slices that add their tests. The scip-java row was confirmed against a strict run with the mise Temurin 25.0.3 on the declared `PATH` and Temurin 17.0.20.1 in the coursier archive cache. The rust-analyzer row was confirmed against a strict run with the Homebrew `rustup` proxy on the declared `PATH`, `RUSTUP_HOME=$HOME/.rustup` and `CARGO_HOME=$HOME/.cargo`: `rustup run 1.98.1 rust-analyzer` reports `rust-analyzer 1.98.1 (48a229ce 2026-09-01)` and the binary under `$RUSTUP_HOME/toolchains/1.98.1-aarch64-apple-darwin/bin/` matched the pinned hash.

### rust-analyzer trusted profile

rust-analyzer runs only in the trusted profile. The restricted profile refuses it with `profile_unsupported`, because `rust-analyzer scip` cannot disable build scripts or proc macros. The trusted run therefore executes the fixture's build scripts and proc macros, and cargo may download crates into `CARGO_HOME`. The command is `rustup run 1.98.1 rust-analyzer scip . --output <out>` with `RUSTUP_HOME`, `CARGO_HOME`, `RUSTUP_AUTO_INSTALL=0` and `CARGO_TARGET_DIR=<run directory>/target`, so build output goes to the run directory and the workspace tree digest is unchanged. The fixture (`tests/fixtures/producers/rust/`) has no dependencies, so the strict run needs no network.

### scip-java JVM prerequisite

Restricted scip-java runs with `COURSIER_MODE=offline` (`src/producer/profiles.rs`). The restricted worktree omits build files and writes an `lsif-java.json` with no `jvm` field, so scip-java uses its default JVM, Temurin 17.0.20.1. That JDK must already be present in `COURSIER_ARCHIVE_CACHE` as a host prerequisite, because Trellis downloads nothing. On the reference host it is at `~/Library/Caches/Coursier/arc/https/github.com/adoptium/temurin17-binaries/releases/download/jdk-17.0.20.1%252B1/OpenJDK17U-jdk_aarch64_mac_hotspot_17.0.20.1_1.tar.gz/jdk-17.0.20.1+1`, created on 2026-08-21. The exact command that provisioned it is not recoverable. It is a coursier archive-cache entry from a prior manual coursier-driven JVM fetch by the owner (for example `cs java --jvm temurin:17` or an earlier online scip-java run), not from anything Trellis ran.

## Declared environment

The CLI sends a closed environment: `PATH`, `HOME`, and `RUSTUP_HOME`, `CARGO_HOME`, `COURSIER_CACHE` and `COURSIER_ARCHIVE_CACHE` when set. The daemon resolves producers only from it. The tests override `HOME` with a temporary directory, so `tests/support/real_producers.rs` computes the rest from the **real** `HOME` first:

- `PATH`: the caller's `PATH` without any `shims` directory, then the non-shim `$HOME/.local/share/mise/installs/node/*/bin` and `.../installs/java/*/bin` directories (real version directories only, not the `temurin-25` style symlinks; newest first), then `/usr/bin` and `/bin`;
- `RUSTUP_HOME=$HOME/.rustup` and `CARGO_HOME=$HOME/.cargo`;
- `COURSIER_CACHE` and `COURSIER_ARCHIVE_CACHE`: `$HOME/Library/Caches/Coursier/{v1,arc}` on macOS and `$HOME/.cache/coursier/{v1,arc}` on Linux.

No shell activation is needed or used: the mise tool directories are added to the declared `PATH` directly, and the shims are never used.

## CI

CI provisions none of these producers. Each real-producer test has a default variant that prints `SKIP producer_unavailable: <reason>` and passes when the producer is missing, unverifiable or a shim, so CI skips them. `./tools/verify` runs the default real-producer tests, and they exercise the real producers when those are available on the host and skip otherwise.

## Evidence

Each producer has a strict `#[ignore]` variant (`..._strict`) that fails with the typed reason instead of skipping and prints one `E2E-EVIDENCE` line after its assertions pass: the pinned version, `verification=verified`, `state=done`, the published revision, the document and fact counts, and `treeDigestUnchanged=true`. Run them on the reference host holding `/tmp/trellis-factory-verify.lock`, with `TRELLIS_SEARCH_MODEL`, `TRELLIS_SEARCH_BACKEND` and `TRELLIS_INDEX_DIAGNOSTICS` unset, and archive the output outside `/tmp`:

```
cargo nextest run --locked --features test-support --no-fail-fast --run-ignored only --success-output immediate --test producer -E 'test(/^producer_e2e(_java|_rust)?::/)'
```

The real-producer tests are the `producer_e2e`, `producer_e2e_java` and `producer_e2e_rust` modules of the `producer` test binary. A strict failure such as `producer_unavailable` is a host provisioning problem: fix the host, never the test.

## Re-pinning

Changing a pin follows [Decision 0017](decisions/0017-conformance-corpus-storage-and-producers.md) item 5: update the profile row (version, `installMethod`, hashes), then this table, and re-run the strict variants. A new version is never accepted by loosening detection.
