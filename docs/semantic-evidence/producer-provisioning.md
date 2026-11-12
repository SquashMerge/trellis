# Producer provisioning (#21, AC7e)

The real-producer end-to-end tests (`tests/producer_e2e*.rs`) run the four pinned producers through `trellis producer plan` and `trellis producer run`. **Trellis never installs on its own.** Producers stay host-installed and are never bundled. Provisioning a producer is a host action by the owner; `trellis producer install <producer>` runs a pinned install command only on that explicit user command (owner ruling 2026-11-10, #880). The index, the daemon, `producer plan`, `run` and `pickup`, automatic runs and every read install nothing. The tests only detect and verify what is already there, and they never run a package manager, `rustup component add`, `coursier bootstrap` or a download; the `producer install` tests use fake launchers on an isolated `PATH`.

## Doctor and install (#880)

`trellis producer doctor [--workspace <dir>] [--json]` is read-only and runs in the CLI, with no daemon or index. It lists the languages a capture of the checkout would admit (by the index's own walk and ignore rules, reading no source), and for each producer, marked `relevant` when it covers one of them, one state:

| State | Meaning |
|---|---|
| `ready` | Resolution and verification succeed exactly as `producer plan` does them: the same declared environment (the caller's `PATH`, `HOME`, `RUSTUP_HOME`, `CARGO_HOME`, `COURSIER_*`), the same `detect::resolve`, with no `--executable`. A `ready` producer's plan verifies. |
| `missing` | The launcher (npm entry point, `scip-java`) or the pinned toolchain's `rust-analyzer` binary is not where resolution looks. |
| `wrongVersion` | The launcher's npm package names another version; `found` and `pinned` give both. |
| `unverifiable` | Something was found but cannot be verified: a shim launcher or `node`, a hash that is not the pin (`scip-java`, `rust-analyzer`), an unreadable or non-UTF-8 path. |
| `prerequisiteMissing` | The producer is there, but a runtime it needs is not: `node`, `java`, `rustup`, or, for scip-java, its default Temurin 17.0.20.1 JDK in `COURSIER_ARCHIVE_CACHE` that restricted runs need (see [scip-java JVM prerequisite](#scip-java-jvm-prerequisite)). |

Doctor always reports. An explicit `--workspace` it cannot resolve is recorded as `workspaceError` (`{code: "workspace_unavailable", message}`), and every producer is listed. An invalid declared environment, for example `HOME` unset, is recorded as `environmentError` (`{code: "environment_invalid", message}`), with no producer checked. Only invalid command-line usage exits non-zero.

The scip-java JDK prerequisite counts only an unpacked JDK whose platform `java` is an executable file: `Contents/Home/bin/java` on macOS, `bin/java` on Linux. An empty or partly unpacked directory, as an interrupted fetch leaves it, is `prerequisiteMissing`.

The check stands for the default profile: `restricted`, or `trusted` for rust-analyzer, which has no restricted row. Each entry lists its prerequisites with `neededFor` (`run`, `install` or `restrictedRun`) and, when it is not ready, the pinned install command. The command comes from the profile row's `installMethod` (the npm producers and scip-java) with a leading `~` expanded to the declared `HOME` and no shell involved, or, for rust-analyzer, whose row records the toolchain in prose, `rustup component add rust-analyzer --toolchain 1.98.1`, as the table below says. The report also says what works without producers: native evidence only. Doctor always exits 0.

`trellis producer install <producer> [--yes] [--json]` is the only code path that runs an install command (`src/producer/install.rs`, the one spawn site the static inventory in `tests/producer/producer_spawn_inventory.rs` allows for it; the same test proves nothing but the CLI's `producer install` reaches it). It:

1. checks the producer with doctor, and runs nothing when it is already `ready`;
2. prints the pinned command, and says when it will create the command's output directory first (`~/.local/bin` for scip-java);
3. refuses with `prerequisite_missing` and runs nothing when a prerequisite of the command is absent: `node` or `npm` for the npm producers, `coursier` for scip-java, `rustup` or the pinned toolchain for rust-analyzer. It never installs a prerequisite;
4. asks for confirmation on a terminal, refuses with `confirmation_required` when stdin is not one, and skips the prompt with `--yes`;
5. creates that output directory if it is absent, then runs exactly that argv, with the program resolved on the declared `PATH`, and with `RUSTUP_AUTO_INSTALL=0` for rustup so adding the component cannot install the toolchain. A non-zero exit is `install_failed`. With `--json`, the command's own output and the progress lines go to stderr, so stdout is exactly one JSON document;
6. runs doctor for that producer again and reports the result. When the launcher was written outside the declared `PATH`, for example `~/.local/bin` is not on it, the entry's `hint` says so.

Pins live in [`hardware/mac-mini-m5-pro-v1.json`](hardware/mac-mini-m5-pro-v1.json). The producer rows record `installMethod` as it was captured. The table below records what the reference host runs today.

## Reference host

| Producer | Pin | Install method | Launcher location class | Verification | Observed runtime |
|---|---|---|---|---|---|
| scip-typescript | 0.4.0 | `npm install -g @sourcegraph/scip-typescript@0.4.0` under a mise-managed node | npm global `bin/` entry, a symlink to the package's `dist/src/main.js`, next to a native `node` | `packageManifestVersion`: `package.json` name and version of the launcher's package equal the pin | node v24.22.1 (the profile text says 24.11.1; the node binary is hashed into the run's provenance, not compared with that text) |
| scip-python | 0.6.6 | `npm install -g @sourcegraph/scip-python@0.6.6` under the same node | npm global `bin/` entry, a symlink to the package's `index.js`, next to a native `node` | `packageManifestVersion` | node v24.22.1 |
| scip-java | 0.12.3 | `coursier bootstrap --standalone com.sourcegraph:scip-java_2.13:0.12.3 -o ~/.local/bin/scip-java` (owner ruling 2026-11-10: the output path is part of the recorded command; the pin is unchanged) | a standalone launcher file on the declared `PATH` | `executableSha256` equals the pinned `executableHash` | mise-installed Temurin 25.0.3 (`~/.local/share/mise/installs/java/temurin-25.0.3+9.0.LTS/bin`) on the declared `PATH`; the scip-java JVM is Temurin 17.0.20.1, see below |
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

`rust-analyzer scip` 1.98.1 does not read a root `rust-analyzer.toml` (probed 2026-11-10: a `cargo.features` set there, in either TOML spelling, left a feature-gated function out of the index, while the same features through `--config-path` put it in). So capture does not retain it and the plan does not hash it.

**scip-typescript.** `typescript.project` is the root `tsconfig.json`, else the root `jsconfig.json`, else `generated` (#652). scip-typescript reads only `tsconfig.json`, so a `jsconfig.json` project is used through a generated `worktree/tsconfig.json` of `{"extends":"./jsconfig.json","compilerOptions":{"allowJs":true}}`: a config reached through `extends` keeps none of TypeScript's `jsconfig.json` defaults, and `allowJs` is the one the program needs. `--ts-project tsconfig.json|jsconfig.json|generated` replaces the choice. A root config the override does not use is left out of the worktree and listed under `omitted`, so it is never read and one over the config input cap does not refuse the run; a `jsconfig.json` that the chosen `tsconfig.json` extends by a relative path is used, so it is kept. An override naming a config the checkout does not hold is refused.

`typescript.projectReferences` lists the project configs scip-typescript reaches through `references` from the chosen root `tsconfig.json`, followed as scip-typescript 0.4.0 follows them: each entry's `path` names a directory holding a `tsconfig.json`, or that file, relative to the config that lists it, and each config reached is followed in turn, once. Capture retains a nested `tsconfig.json` above JavaScript and TypeScript sources (#717), as it does a nested `package.json`, so a referenced config is in the worktree and its hash is in the plan's `config` list and `planSha256`. scip-typescript skips a reference whose config is missing without an error, so the plan refuses with `producer_config_unresolved` instead when:

- a reference leaves the checkout, or names a file other than `tsconfig.json` (such as `tsconfig.app.json`), which capture does not retain;
- a referenced config was not retained, or is over the config input cap;
- `references` is not an array of `{"path": string}` objects, or a referenced config is not a JSON object;
- a referenced config's `extends` names a package (only the root config's package `extends` is supplied, as above);
- the references reach more than 64 configs.

A `jsconfig.json` project follows no references, because TypeScript does not inherit `references` through the generated config's `extends`, and the generated tsconfig has none. `--ts-project` replaces the project and so its references; `--ts-project generated` follows none. A nested `tsconfig.json` that no reference reaches is still in the worktree, unused. scip-typescript's `--yarn-workspaces` and `--pnpm-workspaces` are not derived (owner ruling 2026-11-10); see [known limitations](../known-limitations.md#project-settings-a-producer-plan-does-not-read).

**scip-python.** scip-python 0.6.6 infers its own project name and version from `pyproject.toml` when they are not passed; Trellis always passes them, so it derives them the same way and lists them:

- `python.projectName` and `python.projectVersion`, passed as `--project-name` and `--project-version`: the key of the root `pyproject.toml`'s `[project]` table, or, when `[project]` has no such key, of `[tool.poetry]`, as scip-python reads them. scip-python does not read `setup.cfg`; Trellis passes the value anyway, so for a key neither table sets it falls back to the root `setup.cfg`'s `[metadata]` (#717), read as setuptools' `configparser` reads it: `=` or `:` delimits a key, keys are case-insensitive, full-line `#` and `;` comments are skipped, and a deeper-indented line continues a value. A setuptools `attr:` or `file:` directive is not evaluated and sets nothing. When no file sets a key, the default applies: `trellis-workspace` and `0` (a version is always passed, since without one scip-python runs `git`). A `[project] dynamic = ["version"]` therefore gets `0` unless `[tool.poetry]` or a literal `setup.cfg` version sets one.
- Only the trusted profile reads `pyproject.toml` and `setup.cfg`. The restricted profile omits `pyproject.toml` and `pyrightconfig.json` from the worktree (see the restricted row), since either can configure the interpreter pyright uses; there neither file is read, a malformed one does not refuse, and the defaults apply.
- `--python-project-name` and `--python-project-version` replace the derived value, each for its own key and in either profile. A key that is overridden is not read, and `pyproject.toml` is not parsed when both are.
- Refusals, where scip-python would silently drop the value: a `pyproject.toml` that is not UTF-8 or not TOML, or was over the config input cap; a `setup.cfg` that is not UTF-8, was over the cap, has a line that is not a section, key or comment, a key before any section, or (as `configparser`'s strict mode refuses them) a repeated section or `[metadata]` key; a non-table `project`, `tool` or `tool.poetry` on the way to a key that is read; a name or version that is not a string; a name that is not a PEP 508 project name (ASCII letters, digits, `.`, `_` and `-`, starting and ending with a letter or digit); a version that `packaging`'s PEP 440 `VERSION_PATTERN` does not match, case-insensitively, or that has surrounding whitespace. Names and versions are at most 256 bytes. An override is held to the same rules (`invalid_request`).
- `pyrightconfig.json` is a captured root input (#717) that pyright reads itself from the trusted worktree, as it reads `[tool.pyright]` from `pyproject.toml`; it needs no setting, and its hash is in the plan's `config` list and `planSha256`. The environment stays as the trust profile sets it (an empty `--environment` list when restricted, the declared `python3` when trusted); a checkout's `.venv` is not a retained input and is not detected or activated.

**scip-java.** scip-java detects its build tool from the files in its working directory. Trellis derives the same choice from the retained root inputs and lists it:

- `java.buildTool`, in the trusted profile: `Maven` when the root has `pom.xml`, `Gradle` when it has `build.gradle`, `build.gradle.kts` or `settings.gradle`, and `SCIP` (scip-java's own build tool) when it has `lsif-java.json` (#717), passed as `--build-tool`. As in the pinned scip-java 0.12.3 (probed with `--build-tool trellis-probe`, which reports the detected tools and runs no build), `settings.gradle.kts` or `gradle.properties` alone selects nothing. With no such file the value is `null` and nothing is passed; scip-java then fails with no build tool detected, as it would without Trellis.
- Files of two build tools together refuse with `producer_config_unresolved`, naming the files and the tools; scip-java refuses each such combination too (probed 2026-11-10: `lsif-java.json` with `pom.xml` or `build.gradle` is "Multiple build tools detected: SCIP, Maven" or "SCIP, Gradle"). `--java-build-tool maven|gradle|scip` replaces the detected tool and resolves the conflict. An override naming a tool whose files the checkout lacks is refused.
- The restricted profile omits the root and nested build files (capture retains nested `pom.xml`, `build.gradle` and `build.gradle.kts` above Java sources for app detection, #854) and the checkout's `lsif-java.json`, whose dependencies scip-java would fetch, so `java.buildTool` is `SCIP` (the Trellis-written `lsif-java.json`), nothing is passed, and `--java-build-tool` is `invalid_request`.
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
