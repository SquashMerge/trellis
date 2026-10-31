# Trellis quickstart

Trellis is a local code browser for JavaScript, Rust, Java and Python. It indexes a checkout on your machine, then shows you its architecture, file sizes, classes and static call sequences in a web app, and answers the same questions for coding agents over MCP. It reads your code without running it, and it marks what it can prove and what it can't. Everything stays on your machine.

See [known limitations](known-limitations.md) before you rely on it.

## 1. Install

Release archives are on the [GitHub Releases page](https://github.com/SquashMerge/trellis/releases).

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
trellis --version
```

The archive holds only `trellis` and `LICENSE`. The binaries aren't signed. If you downloaded the archive with a browser on macOS, clear the quarantine flag after checking the checksum: `xattr -d com.apple.quarantine trellis`.

The examples below also use [`jq`](https://jqlang.org) to read Trellis's JSON output.

## 2. Start Trellis and connect the web app

Create a private folder for the token, then start Trellis from your repository:

```sh
mkdir -m 700 -p ~/.trellis-private
cd /path/to/your/repo
trellis serve --token-file ~/.trellis-private/token
```

`serve` prints the address, and indexes the checkout in the background. Then open **http://127.0.0.1:8877/**.

**How the token file works.** The web app needs a token to talk to Trellis. If the file doesn't exist, `serve` creates it with a random token, readable only by you. If it exists, `serve` reuses it. The file must:

- be outside the checkout;
- have a parent folder that already exists;
- be private to you (mode `600`). Otherwise `serve` stops with `insecure token file`.

**To connect,** paste the token (`cat ~/.trellis-private/token`) or choose **Load token file instead** and pick the file, then select **Connect**. **Remember token on this browser** is optional: it keeps the token in this browser's local storage until you select **Disconnect** (under the **⋯** menu).

**Useful `serve` options:**

- `--workspace PATH` serves a checkout other than the current one.
- `--bind 127.0.0.1:9000` uses another port. Only loopback addresses are accepted.

**Stopping.** `serve` is a thin client of a per-user background process, `trellis daemon`, which does the work. Ctrl-C ends `serve`, but the daemon keeps running and the web app keeps working. Running the same `serve` command again attaches to it.

To stop everything, or before you change the port or token file, stop the daemon. Otherwise `serve` fails with `serve bind conflicts with running listener` or `serve token file conflicts with running listener`.

```sh
pgrep -fl 'trellis daemon'    # find the PID and check it's yours
kill -TERM <pid>
```

**More checkouts.** One daemon handles all your checkouts. Run `trellis index` in another checkout, and it appears in the checkout menu at the top of the web app.

**Staying up to date.** Trellis watches each open checkout and refreshes its index a few seconds after you save a file or switch branches. It doesn't run your code or your build to do this.

## 3. What each view shows

The top bar switches between **Diagrams** and **Files**.

**Diagrams** has four tabs:

- **Architecture** draws your layers and the references between them, with arrows that break your rules in red. It needs a [rules file](#6-architecture-rules). Without one, it tells you so.
- **Treemap** shows folders and files sized by lines. Color shows a complexity metric that you choose, or production versus test.
- **Sequence** shows what a method does, step by step: calls, branches and returns. Pick a method from **Most complex methods** or from the file tree. These are static possible paths, not a runtime trace. **Show N incidental calls** reveals the calls hidden by default.
- **Classes** finds a class and shows its declared fields and methods.

**Files** is a file tree with a read-only source view. It shows the indexed version of a file, and uncommitted changes against `HEAD` when you have them.

The **Inspector** panel on the right shows the evidence for whatever you've selected.

## 4. Connect a coding agent with MCP

`trellis mcp` is a read-only MCP server that talks over stdin and stdout. It needs no token, and it starts the daemon if it isn't running, so you don't need `serve` for it.

**Claude Code.** Put this in `.mcp.json` at the root of your repository:

```json
{
  "mcpServers": {
    "trellis": {
      "command": "trellis",
      "args": ["mcp"]
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

On Linux, use the Rust steps under [Feature-gated Rust code](#feature-gated-rust-code) instead: see [known limitations](known-limitations.md#supported-platforms).

**Trust profiles:**

- `--trust restricted` is the default and runs none of your repository's code. Java build files and `pyproject.toml` are left out, and Java runs offline. That means calls into third-party dependencies may stay unresolved.
- `--trust trusted` lets the producer run your repository's build logic, which is not sandboxed. Use it only on code you trust. `rust-analyzer` always needs it, because it can't turn off build scripts and proc macros. Trusted `scip-java` runs your Gradle or Maven build, so that tool must be reachable: the run's `PATH` holds only your JDK's folder, `/usr/bin` and `/bin`.

Every run happens in a private copy of the indexed files. Your checkout isn't touched. The defaults are a 30-minute limit and 8 GiB of memory; change them with `--timeout-secs` and `--memory-mib`, on both commands.

If a run fails, `.diagnostics.stderrTail` and `.diagnostics.stdoutTail` in its output show why.

### Re-run after edits

Semantic evidence describes the code as it was when you imported it. **Any** change to an indexed source file or project config makes the evidence for **every** language [possibly stale](#possibly-stale), including files you didn't touch. Trellis keeps the old results and labels them, but it never re-runs a producer by itself. To refresh, repeat the commands above for each producer you use:

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

Check the rules from the command line:

```sh
trellis rules check | jq .payload.counts
```

The command exits with `1` and prints `Error: architecture deviations` if there are any, so you can use it in a script or CI. The full JSON has each finding under `.payload.instances`. Use `--path 'src/**'` to check only some source files.

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
- **`trellis status`** counts each language's files as complete, partial or failed under `coverage`. `coverage.summary` gives the common reason, such as `partial: 1 · native macro/attribute gaps only`.
- **`failed` under `coverage`** means the imported evidence doesn't cover a file. Usually the file wasn't compiled, as with [feature-gated Rust](#feature-gated-rust-code), or it changed after the import.

The most common cause is Rust macros. A macro Trellis can't see into is shown as a boundary, and the file counts as partial. See [known limitations](known-limitations.md#rust-macros).

### Possibly stale

After an edit to indexed code or config, imported evidence is *possibly stale*: it describes an earlier version of the checkout. It may still be right, but Trellis can't prove it, so it stops using it to draw conclusions until you [re-import](#re-run-after-edits).

- **Web app:** Sequence falls back to `Syntax only · Semantic evidence for <file> is stale…`. The green **Fresh** label in the top bar is about the syntax index only. It doesn't mean imported evidence is current.
- **MCP:** a language's `semantic.freshness` is `possiblyStale`, with `available: false`. Calls keep their old target, marked `freshness: "possiblyStale"`, and aren't followed any further.
- **`trellis status`:** `semanticState` has no separate value for this, and shows `stale`.

`fresh` comes back when you've re-imported.

## Next

- [Known limitations](known-limitations.md)
- [Rules reference](rules.md)
- [Release builds](releases.md)
- [Optional local semantic search](../README.md#local-semantic-search)
