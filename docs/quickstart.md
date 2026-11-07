# Trellis quickstart

Trellis is a local code browser for JavaScript, Rust, Java and Python. It indexes a checkout on your machine, then shows you its architecture, file sizes, classes and static call sequences in a web app, and answers the same questions for coding agents over MCP. It reads your code without running it, and it marks what it can prove and what it can't. Everything stays on your machine.

See [known limitations](known-limitations.md) before you rely on it.

## 1. Install

Check the [GitHub Releases page](https://github.com/SquashMerge/trellis/releases) for published
archives. The commands below use `0.1.0` as an example; they do not mean that version has
been published. Set `VERSION` to an available release. Manual and pull-request workflow
artifacts are candidates, not published releases; see [Release builds](releases.md).

| Platform | Target name |
| --- | --- |
| ARM64 macOS 14 or later, with Metal | `aarch64-apple-darwin` |
| x86-64 Linux, glibc 2.31 or later | `x86_64-unknown-linux-gnu` |
| ARM64 Linux, glibc 2.31 or later | `aarch64-unknown-linux-gnu` |

Download the archive and its checksum, check it, and put the binary on your `PATH`:

```sh
VERSION=0.1.0
TARGET=aarch64-apple-darwin
BASE=https://github.com/SquashMerge/trellis/releases/download/v$VERSION
curl -fLO "$BASE/trellis-$VERSION-$TARGET.tar.gz"
curl -fLO "$BASE/trellis-$VERSION-$TARGET.tar.gz.sha256"

shasum -a 256 -c "trellis-$VERSION-$TARGET.tar.gz.sha256"   # Linux: sha256sum -c
tar xzf "trellis-$VERSION-$TARGET.tar.gz"
mkdir -p ~/.local/bin
install -m 755 trellis ~/.local/bin/trellis
export PATH="$HOME/.local/bin:$PATH"
trellis --version
```

Add the `export` line to your shell's startup file (such as `~/.zshrc`) to keep it.

The archive holds only `trellis` and `LICENSE`. The binaries aren't signed. If you downloaded the archive with a browser on macOS, clear the quarantine flag after checking the checksum: `xattr -d com.apple.quarantine trellis`.

The examples below also use [`jq`](https://jqlang.org) to read Trellis's JSON output.

### Optional local semantic search

For semantic symbol search, install the local embedding model before starting Trellis:

```sh
trellis model install
```

This explicit command needs network access on first install. It downloads and verifies
the reviewed `voyageai/voyage-4-nano` model into a shared user-wide directory, so you
only need to install it once across checkouts. Defaults work without a configuration file
or model environment variables, including for Claude Desktop. Backend selection is
automatic: official macOS ARM64 releases support Metal with CPU fallback; Linux
releases use CPU.

The model is optional for the browser and lexical symbol search, but required for
vector-based semantic symbol search. Trellis never downloads weights implicitly.
If a daemon is already running, install prints a restart hint. Close active MCP clients,
then run `trellis daemon stop` and start Trellis again to load the model. Restart any
`trellis serve` command too. Model loading and vector coverage build in the background;
lexical results stay available. See the [restart steps](semantic-search-local-cache.md#configuration-and-daemon-restart).
See the [local model guide](semantic-search-local-cache.md) for optional configuration
and availability diagnostics.

## 2. Start Trellis and connect the web app

Create a private folder for the token, then start Trellis from your repository:

```sh
mkdir -m 700 -p ~/.trellis-private
cd /path/to/your/repo
trellis serve --token-file ~/.trellis-private/token
```

`serve` prints the address. Then open **http://127.0.0.1:8877/**. Trellis indexes the checkout in the background once the web app, a CLI command or an MCP client first uses it.

**How the token file works.** The web app needs a token to talk to Trellis. If the file doesn't exist, `serve` creates it with a random token, readable only by you. If it exists, `serve` reuses it. The file must:

- be outside the checkout;
- have a parent folder that already exists;
- be private to you (mode `600`). Otherwise `serve` stops with `insecure token file`.

**To connect,** paste the token (`cat ~/.trellis-private/token`) or choose **Load token file instead** and pick the file, then select **Connect**. **Remember token on this browser** is optional: it keeps the token in this browser's local storage until you select **Disconnect** (under the **⋯** menu).

**Useful `serve` options:**

- `--workspace PATH` serves a checkout other than the current one.
- `--bind 127.0.0.1:9000` uses another port. Only loopback addresses are accepted.

**Stopping.** `serve` is a thin client of a per-user background process, `trellis daemon`, which does the work. Ctrl-C ends `serve`, but the daemon keeps running and the web app keeps working. Running the same `serve` command again attaches to it. While `serve` runs, its control connection keeps the daemon alive even without an open browser. After the last `serve` stops, the daemon may exit after its ordinary 30-minute idle grace if no other clients or work remain.

To stop everything, or before you change the port or token file, stop the daemon. Otherwise `serve` fails with `serve bind conflicts with running listener` or `serve token file conflicts with running listener`.

```sh
trellis daemon stop
```

Stop waits for accepted indexing or import work to finish and for the old daemon
to release ownership. It can take longer than a minute; there is no completion
read timeout.

### Keep a live serve detached

For a long-running live instance, do not leave `serve` owned by an agent's tool
shell. Use both `nohup` (ignore terminal hangup) and a new session (detach from
the shell's process group), with stdin and both output streams redirected.
Use an installed binary and absolute paths, not `cargo run`.

On **Linux**, `setsid` is provided by util-linux:

```sh
mkdir -m 700 -p "$HOME/.trellis-private"
nohup setsid /absolute/path/to/trellis serve \
  --workspace /absolute/path/to/checkout \
  --token-file "$HOME/.trellis-private/token" \
  </dev/null >"$HOME/.trellis-private/serve.log" 2>&1 &
```

**macOS does not ship `setsid`.** With Python 3 installed, create the session
before replacing the launcher with Trellis:

```sh
mkdir -m 700 -p "$HOME/.trellis-private"
nohup python3 -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' \
  /absolute/path/to/trellis serve \
  --workspace /absolute/path/to/checkout \
  --token-file "$HOME/.trellis-private/token" \
  </dev/null >"$HOME/.trellis-private/serve.log" 2>&1 &
```

Check `serve.log` for the readiness banner. `nohup` alone does not create a new
session, and `&` alone does not detach it. Detachment does not prevent a tool
that explicitly kills all its descendants from stopping it; use a persistent
terminal or a user service manager in that environment.

`serve` logs `SIGTERM` or `SIGINT` when explicitly stopped and exits successfully.
If its daemon control connection closes independently, even after a normal daemon
exit, it logs `daemon_unavailable` and exits nonzero. EOF cannot reveal the
daemon's OS exit status: `serve` may have attached to an already-running daemon.
The log says the status is unavailable rather than guessing it.

**More checkouts.** One daemon handles all your checkouts. Run `trellis index` in another checkout, and it appears in the checkout menu at the top of the web app.

**Staying up to date.** Trellis watches each open checkout and refreshes its index a few seconds after you save a file or switch branches. It doesn't run your code or your build to do this.

## 3. What each view shows

The top bar switches between **Diagrams** and **Files**.

**Diagrams** has four tabs:

- **Architecture** draws your layers and the references between them, with arrows that break your rules in red. It needs a [rules file](#6-architecture-rules). Without one, it tells you so.
- **Treemap** shows folders and files sized by lines. Color shows a complexity metric that you choose, or production versus test.
- **Sequence** shows what a method does, step by step: calls, branches and returns. Pick a method from **Most complex methods** or from the file tree. These are static possible paths, not a runtime trace. **Show all steps** restores admitted detail and incidental calls; **Show calls** returns to the calls-first view.
- **Classes** finds a class and shows its declared fields and methods.

**Files** is a file tree with a read-only source view. It shows the indexed version of a file, and uncommitted changes against `HEAD` when you have them.

The **Inspector** panel on the right shows the evidence for whatever you've selected.

## 4. Connect a coding agent with MCP

`trellis mcp` is a read-only MCP server that talks over stdin and stdout. It needs no token, and it starts the daemon if it isn't running, so you don't need `serve` for it.

**Claude Code.** Put this in `.mcp.json` at the root of your repository. Replace
both example paths with the absolute binary and checkout paths on your machine:

```json
{
  "mcpServers": {
    "trellis": {
      "command": "/absolute/path/to/trellis",
      "args": ["mcp", "--workspace", "/absolute/path/to/repo"]
    }
  }
}
```

Start `claude` in the repository and approve the server when asked. `claude mcp list` shows its status.

**Claude Desktop.** Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (**Settings → Developer → Edit Config** opens it), then restart the app. Desktop doesn't start in your repository and doesn't use your shell's `PATH`, so use absolute paths for both the binary and the checkout:

```json
{
  "mcpServers": {
    "trellis": {
      "command": "/Users/you/.local/bin/trellis",
      "args": ["mcp", "--workspace", "/Users/you/path/to/repo"]
    }
  }
}
```

**In use.** The first call on a new checkout can return `index_not_ready` while it indexes. It's safe to retry after a few seconds. Agents get tools for finding symbols, reading source, following calls and references, and checking architecture rules. `trellis_workspace_describe` is the place to start: it reports what evidence each language has.

## 5. Add semantic evidence

Out of the box, Trellis knows what your code *says*: which functions exist and which calls are written where. That's **syntax evidence**. It doesn't know which function a call really reaches.

For that, run a language's own indexer, called a *producer*, and import the result. That gives you **semantic evidence**: resolved call targets, better Sequence diagrams, and more accurate Architecture references. Producers are optional, and you add them per language.

### Install the producers

Trellis accepts only the exact versions below, and never installs them for you.

| Language | Producer | Version | Install |
| --- | --- | --- | --- |
| JavaScript | `scip-typescript` | 0.4.0 | `npm install -g @sourcegraph/scip-typescript@0.4.0` |
| Python | `scip-python` | 0.6.6 | `npm install -g @sourcegraph/scip-python@0.6.6` |
| Java | `scip-java` | 0.12.3 | `coursier bootstrap --standalone com.sourcegraph:scip-java_2.13:0.12.3 -o ~/.local/bin/scip-java` |
| Rust | `rust-analyzer` | 1.98.1 | `rustup toolchain install 1.98.1 --component rust-analyzer` |

- The npm producers need Node.
- `scip-java` needs a JDK on your `PATH`. It also needs its own JDK in the Coursier cache, because Trellis runs it offline by default. To fetch that once (about 500 MB), run it by hand in an empty folder:

  ```sh
  mkdir /tmp/scip-java-setup && cd /tmp/scip-java-setup
  echo '{}' > lsif-java.json
  echo 'class Hello {}' > Hello.java
  scip-java index
  ```

- Version-manager shims (mise, asdf, pyenv) are refused. Put the real install's `bin` folder on your `PATH`, or pass the real file with `--executable` to both `plan` and `run`.

Check what Trellis can see, from inside your repository:

```sh
trellis producer status | jq '.producers[] | {name, detection}'
```

Each producer should show `"result": "available"`. Otherwise `reason` says what's wrong, for example `version 0.4.1 != pinned 0.4.0`.

### Run a producer

A run has two steps. `plan` shows exactly what would run and prints a hash, and it runs nothing. `run` repeats the plan and goes ahead only if the hash still matches.

```sh
cd /path/to/your/repo
PLAN=$(trellis producer plan --producer scip-typescript | jq -r .planSha256)
trellis producer run --producer scip-typescript --plan-sha256 "$PLAN" | jq '{state, outcome, errorCode}'
```

Success is `"state": "done"` and `"outcome": "imported"`. Use the same commands with `scip-python` and `scip-java`.

Rust must use `--trust trusted`, in both commands:

```sh
PLAN=$(trellis producer plan --producer rust-analyzer --trust trusted | jq -r .planSha256)
trellis producer run --producer rust-analyzer --trust trusted --plan-sha256 "$PLAN" | jq '{state, outcome, errorCode}'
```

**Trust profiles:**

- `--trust restricted` is the default and runs none of your repository's code. Java build files and `pyproject.toml` are left out, and Java runs offline. That means calls into third-party dependencies may stay unresolved.
- `--trust trusted` lets the producer run your repository's build logic, which is not sandboxed. Use it only on code you trust. `rust-analyzer` always needs it, because it can't turn off build scripts and proc macros. Trusted `scip-java` runs your Gradle or Maven build, so that tool must be reachable: the run's `PATH` holds only your JDK's folder, `/usr/bin` and `/bin`.

Every run happens in a private copy of the indexed files. Your checkout isn't touched. The defaults are a 30-minute limit and 8 GiB of memory; change them with `--timeout-secs` and `--memory-mib`, on both commands.

If a run fails, `.diagnostics.stderrTail` and `.diagnostics.stdoutTail` in its output show why.

### Re-run after edits

Semantic evidence describes the code as it was when you imported it. **Any** change to an indexed source file or project config affects imported evidence for **every** language: the changed file's evidence is [stale](#stale-and-possibly-stale), and every other file's is [possibly stale](#stale-and-possibly-stale). Trellis keeps the old results for unchanged files and labels them, but it never re-runs a producer by itself. To refresh, repeat the commands above for each producer you use:

```sh
for P in scip-typescript scip-python scip-java; do
  PLAN=$(trellis producer plan --producer $P | jq -r .planSha256)
  trellis producer run --producer $P --plan-sha256 "$PLAN" | jq -c '{producer, state, outcome}'
done
```

You always need a new plan. An old hash fails with `plan_changed`. If you edit a file during a run, it fails with `stale_basis`, and you run it again.

### Feature-gated Rust code

`trellis producer run` analyzes Rust with default Cargo features only. Code behind a feature, such as a `test-support` feature, isn't compiled, so it gets no semantic evidence. Such files show up as `failed` in the coverage counts.

To turn features on, run `rust-analyzer` yourself and have Trellis pick up the result. Keep the config and the output outside your checkout:

```sh
mkdir -p ~/trellis-scip
echo '{"cargo":{"features":["test-support"]}}' > ~/trellis-scip/rust-analyzer.json

cd /path/to/your/repo
CARGO_TARGET_DIR=~/trellis-scip/target \
  rustup run 1.98.1 rust-analyzer scip . \
  --config-path ~/trellis-scip/rust-analyzer.json \
  --output ~/trellis-scip/rust.scip

trellis producer pickup --producer rust-analyzer --artifact ~/trellis-scip/rust.scip | jq '{state, errorCode}'
```

`pickup` imports the file now and watches it from then on. To refresh after edits, re-run only the `rust-analyzer` command, and Trellis imports the new file a few seconds later. This runs your build scripts and proc macros, like `--trust trusted`.

The file must come from `rust-analyzer` 1.98.1: another version fails with `invalid_artifact: tool binding`. To stop watching, run `trellis producer pickup --producer rust-analyzer --clear`.

### Upgrading Trellis

An upgrade can rebuild the native index when its schema or extractor changes. Trellis keeps the last successfully imported `producer run` input directory for each producer, in its original private location. After a rebuild it tries that batch again through normal validation, joining and publication. This does not run the producer.

If the inputs are missing or no longer match, status, the web app and MCP show **semantic evidence needs re-import**, with a per-language reason. Run the producer again using the commands above, or refresh your configured pickup artifact. A queued import does not clear the warning; successful validated publication does. A retained batch alone never means evidence is fresh.

The first upgrade onto this retention behavior also needs re-import for languages a producer covered (a successful `producer run` or a configured pickup): older versions deleted successful run inputs. A language that never had a producer is reported as not configured. Trellis does not migrate old index layouts to recover them. Retained inputs are disposable and GC may remove them; keep any artifact you need to archive outside Trellis. Configured pickup keeps its existing artifact-watch behavior.

## 6. Architecture rules

Rules live in one reviewed file, `.trellis/rules.toml`, in your repository. Trellis never writes it unless you confirm a change in the web app. A small example:

```toml
version = 1

[layers.api]
order = 1
paths = ["src/api/**"]

[layers.core]
order = 2
paths = ["src/core/**"]

[layers.shared]
shared = true
paths = ["src/model.rs"]

[layers.tooling]
order = 1
display = "hidden"
paths = ["tools/**", "build.rs"]
```

- **Layers** select files with `paths`. A lower `order` is higher in the stack.
- References going **down** the stack are fine. References going **up** are deviations.
- Two layers with the same `order` are peers, and references between them are deviations.
- A `shared = true` layer can be used by anyone, but references out of it into ordered layers are deviations.
- Files in no layer stay visible and count as `unknown`, not as passing.

`display` sets how the Architecture tab draws a layer:

- `"expanded"` starts open.
- `"collapsed"` starts as a summary. This is the default.
- `"hidden"` leaves the layer out of the drawing and lists it in a note below, where you can show it. Use it for scripts, build helpers and fixtures. Hidden layers are also left out of the **Most complex methods** list. `display` only affects the web app: rule checks still cover the layer.

Check the rules from the command line. This prints a JSON report, with a summary under `.payload.counts` and each finding under `.payload.instances`:

```sh
trellis rules check
```

If there are deviations, the command prints `Error: architecture deviations` and exits with `1`. It also exits with `1` if the check can't run, for example with no rules file. To fail a script or CI step, don't pipe the command: a pipeline normally returns the exit status of its last command, so `trellis rules check | jq ...` passes even with deviations. Save the report first instead:

```sh
trellis rules check > rules-report.json
```

Use `--path 'src/**'` to check only some source files.

Agents can use `trellis_rules_check` and `trellis_rules_explain`, and the `trellis_propose_layers` MCP prompt drafts layers for you to review. See the [rules reference](rules.md) for exceptions and external package rules.

## 7. Read the results

### Syntax or semantic

Every answer says which kind of evidence it's built on.

- **Syntax:** measured from your source text. A call is real source text, but its target is unverified.
- **Semantic:** a call target resolved by a producer, for a version of the code that matches.

**In the web app,** the Sequence header says `Semantic bindings · rust-analyzer`, or `Syntax only` and why. Calls without a verified target are labelled, for example `measured syntax; target unverified`.

**With MCP,** each call has an `evidenceTier` of `syntax` or `semantic`. Symbol search results are always `syntax`, even in a language with imported evidence.

**At the command line:**

```sh
trellis status | jq '.semanticLanguages[] | {language, semanticState, semanticProducerId, coverage: .coverage.summary}'
```

### Partial

`partial` means Trellis knows something is missing, not that something is wrong. The results you got are valid, but they may not be all of them, so don't read a missing call or reference as proof that there is none.

- **MCP answers** have `partial: true` when the answer was cut off, or when any searched file has incomplete evidence. `warnings` and the per-language coverage counts say which.
- **`trellis status`** counts each language's files as complete, partial or failed under `coverage`. `coverage.summary` gives the common reasons, failed first, such as `partial: 1 · native macro/attribute gaps only` or `failed: 2 · not in producer output; partial: 1 · native macro/attribute gaps only`.
- **`failed` under `coverage`** means the imported evidence doesn't cover a file. Usually the file wasn't compiled, as with [feature-gated Rust](#feature-gated-rust-code), or it changed after the import.

The most common cause is Rust macros. A macro Trellis can't see into is shown as a boundary, and the file counts as partial. See [known limitations](known-limitations.md#rust-macros).

### Stale and possibly stale

Imported evidence describes an earlier version of your code as soon as you edit an indexed source file or project config. Until you [re-import](#re-run-after-edits), it has one of two states:

- **Stale:** this file changed since the import, so its evidence no longer applies. The file counts as `failed` under `coverage`, and its calls go back to syntax.
- **Possibly stale:** this file didn't change, but something around it did. Its evidence may still be right, but Trellis can't prove it, so it stops drawing conclusions from it.

Where you'll see this:

- **Web app:** Sequence falls back to `Syntax only · Semantic evidence for <file> is stale…` in both cases. The green **Fresh** label in the top bar is about the syntax index only. It doesn't mean imported evidence is current.
- **MCP, for a call in a changed file:** `evidenceTier` is `syntax`, with `boundaryReason: "missingEvidence"`. If an answer does return old evidence for a changed file, it's marked `freshness: "stale"`.
- **MCP, for a call in an unchanged file:** the call keeps its old target, marked `freshness: "possiblyStale"`, and isn't followed any further.
- **MCP, for a language:** `semantic.freshness` is the worst state among the files it still covers, usually `possiblyStale`, with `available: false`.
- **`trellis status`:** `semanticState` shows `stale` in both cases.

`fresh` comes back when you've re-imported.

## Dogfood an installed candidate

For a release candidate or a real-repository MCP check, follow the
[installed-binary smoke steps](releases.md#installed-binary-mcp-dogfood). The runner
uses your installed binary and chosen checkout, checks every live catalog tool,
and reports errors and missing capabilities without printing tokens. It needs
this project's tooling checkout and Node dependencies; it is not bundled in the
release archive.

## Next

- [Known limitations](known-limitations.md)
- [Rules reference](rules.md)
- [Release builds](releases.md)
- [Optional local semantic search](../README.md#local-semantic-search)
