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

rust-analyzer runs only in the trusted profile. The restricted profile refuses it with `profile_unsupported`, because `rust-analyzer scip` cannot disable build scripts or proc macros. The trusted run therefore executes the fixture's build scripts and proc macros, and cargo may download crates into `CARGO_HOME`. The command is `rustup run 1.98.1 rust-analyzer scip . --output <out>`, plus `--config-path <run>/producer-config/rust-analyzer.json` when the plan enables Cargo features (see [Project settings](#project-settings)), with `RUSTUP_HOME`, `CARGO_HOME`, `RUSTUP_AUTO_INSTALL=0` and `CARGO_TARGET_DIR=<run directory>/target`, so build output goes to the run directory and the workspace tree digest is unchanged. The fixture (`tests/fixtures/producers/rust/`) has no dependencies, so the strict run needs no network.

### scip-typescript project config

scip-typescript runs `index --output <out>` in the run's worktree, with no `--infer-tsconfig` (owner ruling 2026-11-03, #652). If the checkout has no root `tsconfig.json`, the plan generates `worktree/tsconfig.json` as `{"compilerOptions":{"allowJs":true,"jsx":"preserve","target":"es2022","module":"commonjs"}}` and lists it under `generatedConfig`. That is what `--infer-tsconfig` would write, except for the ES2022 target: the inferred ES5 target can't parse ES2015+ identifiers, and an escaped astral function name in `tests/fixtures/semantic-evidence/v1/javascript` became a zero-width definition. `commonjs` keeps the module resolution the inferred config had. `allowJs` keeps JavaScript and TypeScript sources in one program, and `jsx: preserve` lets `.tsx` files parse. One scip-typescript run covers both languages: its pin lists `javascript` and `typescript`, the artifact's descriptor entry lists the languages it covers, and an empty SCIP `Document.language` binds to each document's own detected language. A checkout's own root `tsconfig.json` replaces the generated one. Nothing is written into the checkout.

The worktree holds no `node_modules`, so TypeScript cannot resolve a root `tsconfig.json` whose `extends` names an npm package (TS6053, #799). The plan handles such an `extends` in this order, under either trust profile, because both use the same worktree:

1. **Supply the base config.** Trellis looks the package up in the checkout's own `node_modules` as TypeScript 5.9 does for a config's `extends`. It checks the `node_modules` of the extending config's directory, then of each ancestor up to the checkout root. A package directory with no config passes the lookup outward, unless its `exports` decided it. Inside a package, the order is:
   - the `exports` entry for `.` or the subpath, with conditions matched in the manifest's own order (`require`, `types`, `node` and `default` match, and a target that is missing passes to the next);
   - else the `tsconfig` field of `package.json`;
   - else `tsconfig.json`.

   A subpath also tries `<subpath>.json` and `<subpath>/tsconfig.json`. Trellis reads that config and every config its `extends` chain reaches, within these bounds:
   - only regular files whose real path, after symlinks, is under the checkout's `node_modules` are read;
   - each file is at most 256 KiB;
   - one resolution reads at most 16 files, configs and `package.json` manifests together, and inspects at most 256 paths;
   - nothing is executed.

   Each config is generated at its own path under `worktree/node_modules/`, so relative options in it keep their meaning, and the root's package `extends` is rewritten to a relative path. The plan lists the root under `omitted` with the packages it extends, and lists the rewritten root and every supplied file under `generatedConfig`, so a change to any of them changes `planSha256`. A file with no package `extends` is supplied byte for byte.
2. **Fall back to the generated tsconfig.** When the package or its config file is absent from `node_modules`, for example because dependencies are not installed, the generated tsconfig above replaces the repo's (owner ruling #652, `default_only`). The plan lists `tsconfig.json` under `omitted`, with a reason that names the `extends` and says it was replaced. Only absence falls back.
3. **Typed refusal.** Any other failure of an `extends` refuses `producer plan` and `producer run` with `producer_config_unresolved`. The message names the `extends` and the reason. Such failures include:
   - a permission or other I/O error;
   - a config path that is not a regular file;
   - a real path outside `node_modules`;
   - a malformed config or `package.json`;
   - a cycle, a file over the cap, or a resolution over either bound;
   - `exports` subpath patterns or versioned `types@` conditions, which are not supported.

A relative `extends` in the root config is left to TypeScript. A root config Trellis cannot parse is refused with `producer_config_unresolved` (#717). The project config is chosen as [Project settings](#project-settings) describes; the same `extends` handling applies to a root `jsconfig.json`.

### Project settings

A plan reads each producer's settings from the project's own files (#717), read-only and bounded, and executes nothing to find them: no build, build script, package manager or network fetch. The plan's `settings` list gives each effective setting as `{name, value, source, from, detail}`. `source` is `project` (read from the files in `from`, with their retained hashes), `override` (given on the command) or `default` (nothing sets it). The settings and the files they came from are part of `planSha256`, so `run` refuses with `plan_changed` unless it derives the same settings.

An explicit override on `producer plan` and `producer run` replaces the derived value; it is never merged with it. Unparseable or contradictory project config refuses `plan` and `run` with `producer_config_unresolved`, naming the file and the reason; it never falls back to defaults.

**rust-analyzer.**

- `cargo.workspaceMembers`: the root package, and each retained nested `Cargo.toml` that the root `[workspace] members` globs match and `exclude` does not. A manifest outside the members, such as a test fixture, is not read.
- `cargo.features`: the union of the `required-features` of every member's `[[bin]]`, `[[test]]`, `[[bench]]` and `[[example]]` targets. In a workspace of more than one package, a feature is qualified as `package/feature`. Nothing else is enabled: never all features, so a platform feature such as `metal` stays off. When the list is non-empty, the plan generates `producer-config/rust-analyzer.json` as `{"cargo":{"features":[...]}}`, lists it under `generatedConfig`, and passes it with `--config-path`. `--cargo-features a,b` replaces the list, and `--cargo-features ''` enables none.
- Refusals: a root or member manifest that is not UTF-8 or not TOML, or was over the config input cap; a non-table `[workspace]` or `[package]`; members, exclude or `required-features` that are not arrays of strings; an invalid member glob; a member without `[package]` or a package name; a target table that is not an array of tables; a required feature that is not a Cargo feature name, or that `[features]` and the optional dependencies do not declare; more than 256 features. A Cargo feature name follows Cargo's own rule: a Unicode XID start character, `_` or an ASCII digit, then Unicode XID continue characters, `-`, `+` or `.`, at most 128 bytes. As in Cargo, an optional dependency declares an implicit feature of its name only while no `[features]` value names it as `dep:<name>`.

Whether `rust-analyzer scip` 1.98.1 reads a root `rust-analyzer.toml` is not verified, so the plan does not read it.

**scip-typescript.** `typescript.project` is the root `tsconfig.json`, else the root `jsconfig.json`, else `generated` (#652). scip-typescript reads only `tsconfig.json`, so a `jsconfig.json` project is used through a generated `worktree/tsconfig.json` of `{"extends":"./jsconfig.json","compilerOptions":{"allowJs":true}}`: a config reached through `extends` keeps none of TypeScript's `jsconfig.json` defaults, and `allowJs` is the one the program needs. `--ts-project tsconfig.json|jsconfig.json|generated` replaces the choice. A root config the override does not use is left out of the worktree and listed under `omitted`, so it is never read and one over the config input cap does not refuse the run; a `jsconfig.json` that the chosen `tsconfig.json` extends by a relative path is used, so it is kept. An override naming a config the checkout does not hold is refused.

TypeScript project references and nested `tsconfig.json` files are not followed. A nested config is not a captured root input, so supporting it needs a capture change that is also an extractor-hash input (`src/native_evidence.rs`); it waits for that decision.

**scip-python.** scip-python 0.6.6 infers its own project name and version from `pyproject.toml` when they are not passed; Trellis always passes them, so it derives them the same way and lists them:

- `python.projectName` and `python.projectVersion`, passed as `--project-name` and `--project-version`: the key of the root `pyproject.toml`'s `[project]` table, or, when `[project]` has no such key, of `[tool.poetry]`, as scip-python reads them. When neither sets a key, the default applies: `trellis-workspace` and `0` (a version is always passed, since without one scip-python runs `git`). A `[project] dynamic = ["version"]` therefore gets `0` unless `[tool.poetry]` sets a version.
- Only the trusted profile reads `pyproject.toml`. The restricted profile omits it from the worktree (see the restricted row), so it is not an input of a restricted run: it is not read, a malformed one does not refuse, and the defaults apply.
- `--python-project-name` and `--python-project-version` replace the derived value, each for its own key and in either profile. A key that is overridden is not read, and `pyproject.toml` is not parsed when both are.
- Refusals, where scip-python would silently drop the value: a `pyproject.toml` that is not UTF-8 or not TOML, or was over the config input cap; a non-table `project`, `tool` or `tool.poetry` on the way to a key that is read; a name or version that is not a string; a name that is not a PEP 508 project name (ASCII letters, digits, `.`, `_` and `-`, starting and ending with a letter or digit); a version that `packaging`'s PEP 440 `VERSION_PATTERN` does not match, case-insensitively, or that has surrounding whitespace. Names and versions are at most 256 bytes. An override is held to the same rules (`invalid_request`).
- Not derived: `setup.cfg` and `pyrightconfig.json` are not captured root inputs, so they cannot be read without the capture change above. `[tool.pyright]` is read by pyright itself from the trusted worktree's `pyproject.toml` and needs no setting. The environment stays as the trust profile sets it (an empty `--environment` list when restricted, the declared `python3` when trusted); a checkout's `.venv` is not a retained input and is not detected or activated.

**scip-java.** scip-java detects its build tool from the files in its working directory. Trellis derives the same choice from the retained root inputs and lists it:

- `java.buildTool`, in the trusted profile: `Maven` when the root has `pom.xml`, `Gradle` when it has `build.gradle`, `build.gradle.kts` or `settings.gradle`, passed as `--build-tool`. As in the pinned scip-java 0.12.3 (probed with `--build-tool trellis-probe`, which reports the detected tools and runs no build), `settings.gradle.kts` or `gradle.properties` alone selects nothing. With no such file the value is `null` and nothing is passed; scip-java then fails with no build tool detected, as it would without Trellis.
- Maven and Gradle files together refuse with `producer_config_unresolved`, naming the files; scip-java refuses that combination too. `--java-build-tool maven|gradle` replaces the detected tool and resolves the conflict. An override naming a tool whose files the checkout lacks is refused.
- The restricted profile omits the build files, so `java.buildTool` is `SCIP` (the Trellis-written `lsif-java.json`), nothing is passed, and `--java-build-tool` is `invalid_request`.
- Not derived: the JDK version and the Gradle or Maven modules. Gradle build files are code, and Maven's come from XML with parent POMs and property interpolation; neither is read safely without running the build. The trusted build applies its own toolchain and modules. The restricted JVM stays scip-java's default, a host prerequisite (see below), so a project's declared JDK cannot make an offline run fetch another one.

### Settings and automatic re-runs (#660)

Settings-derived arguments (Cargo features, the TypeScript project, the Python project name and version, the Java build tool) are excluded from the approval's `templateSha256` (owner ruling 2026-11-08 on #660, D3), so editing them neither widens trust nor needs a new approval. An automatic run re-derives the settings at the current head, without the manual overrides (`--cargo-features` and the others are not stored in the approval); a run therefore uses the project's own files as a plain `plan` does. The template hash is over exactly these members: `templateVersion` (1); `producer` `{name, languages, version, pinnedExecutableHash, positionEncoding, launcher, verification}`; `trustProfile`; `runtimeExecutables`; `environment`; `limits`; and `generation`. It excludes `workspace`, `command` (argv, cwd, env), `settings`, `inputs`, `policy` and `output`.

### scip-java JVM prerequisite

Restricted scip-java runs with `COURSIER_MODE=offline` (`src/producer/profiles.rs`). The restricted worktree omits build files and writes an `lsif-java.json` with no `jvm` field, so scip-java uses its default JVM, Temurin 17.0.20.1. That JDK must already be present in `COURSIER_ARCHIVE_CACHE` as a host prerequisite, because Trellis downloads nothing. On the reference host it is at `~/Library/Caches/Coursier/arc/https/github.com/adoptium/temurin17-binaries/releases/download/jdk-17.0.20.1%252B1/OpenJDK17U-jdk_aarch64_mac_hotspot_17.0.20.1_1.tar.gz/jdk-17.0.20.1+1`, created on 2026-08-21. The exact command that provisioned it is not recoverable. It is a coursier archive-cache entry from a prior manual coursier-driven JVM fetch by the owner (for example `cs java --jvm temurin:17` or an earlier online scip-java run), not from anything Trellis ran.

## Declared environment

The CLI sends a closed environment: `PATH`, `HOME`, and `RUSTUP_HOME`, `CARGO_HOME`, `COURSIER_CACHE` and `COURSIER_ARCHIVE_CACHE` when set. The daemon resolves producers only from it. The tests override `HOME` with a temporary directory, so `tests/support/real_producers.rs` computes the rest from the **real** `HOME` first:

- `PATH`: the caller's `PATH` without any `shims` directory, then the non-shim `$HOME/.local/share/mise/installs/node/*/bin` and `.../installs/java/*/bin` directories (real version directories only, not the `temurin-25` style symlinks; newest first), then `/usr/bin` and `/bin`;
- `RUSTUP_HOME=$HOME/.rustup` and `CARGO_HOME=$HOME/.cargo`;
- `COURSIER_CACHE` and `COURSIER_ARCHIVE_CACHE`: `$HOME/Library/Caches/Coursier/{v1,arc}` on macOS and `$HOME/.cache/coursier/{v1,arc}` on Linux.

No shell activation is needed or used: the mise tool directories are added to the declared `PATH` directly, and the shims are never used.

**Automatic re-runs (#660).** The approval that lets Trellis re-run a producer automatically stores the environment of the manual run that created it (the same closed environment, as detected for that run), together with the trust profile, limits and executable; the owner replays it, it never detects a new one (owner ruling 2026-11-08, D2).

## CI

CI provisions none of these producers. Each real-producer test has a default variant that prints `SKIP producer_unavailable: <reason>` and passes when the producer is missing, unverifiable or a shim, so CI skips them. `./tools/verify` runs the default real-producer tests, and they exercise the real producers when those are available on the host and skip otherwise.

## Evidence

Each producer has a strict `#[ignore]` variant (`..._strict`) that fails with the typed reason instead of skipping and prints one `E2E-EVIDENCE` line after its assertions pass: the pinned version, `verification=verified`, `state=done`, the published revision, the document and fact counts, and `treeDigestUnchanged=true`. Run them on the reference host holding `/tmp/trellis-factory-verify.lock`, with `TRELLIS_SEARCH_MODEL`, `TRELLIS_SEARCH_BACKEND` and `TRELLIS_INDEX_DIAGNOSTICS` unset, and archive the output outside `/tmp`:

```
cargo nextest run --locked --features test-support --no-fail-fast --run-ignored only --success-output immediate --test producer -E 'test(/^producer_e2e(_java|_rust)?::/)'
```

The real-producer tests are the `producer_e2e`, `producer_e2e_java` and `producer_e2e_rust` modules of the `producer` test binary. A strict failure such as `producer_unavailable` is a host provisioning problem: fix the host, never the test.

## Re-pinning

Changing a pin follows [Decision 0017](decisions/0017-conformance-corpus-storage-and-producers.md) item 5: update the profile row (version, `installMethod`, hashes), then this table, and re-run the strict variants. A rust-analyzer re-pin records every target in `targetPins`, as [Amendment 6](decisions/0017-conformance-corpus-storage-and-producers.md#amendment-6-2026-10-31-one-rust-analyzer-pin-per-target) requires. A new version is never accepted by loosening detection.
