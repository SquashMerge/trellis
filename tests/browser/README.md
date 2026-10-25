# Browser acceptance: sequence and shell

This test drives the real Trellis web shell in Playwright Chromium. It starts the compiled daemon with an isolated workspace, token, home, and XDG directories. It does not mock or intercept application requests.

## Verify from a clean checkout

From the repository root, run `./tools/verify`. Its sequential setup step installs the
locked `tests/mcp` and `tests/browser` Node packages when missing or stale, then checks
and repairs the pinned Playwright Chromium bundles. Setup completes before the parallel verification
lanes start. The package lock pins `playwright` to exactly `1.64.1`; setup never uses `npx`,
a system browser, or `--with-deps`. A first run needs npm registry and browser-download access.

By default, setup uses an owner-private, stable **per-checkout** cache under the OS temp
directory, outside the repository. Set `TRELLIS_RUN_CACHE` to an absolute, user-owned,
non-group-writable external directory to keep the npm and browser cache at a known location.
An existing absolute external `PLAYWRIGHT_BROWSERS_PATH` (including CI's) is honored when
it has the same safe ownership and permissions. Invalid, symlinked, or in-checkout cache
paths fail verification; neither missing packages nor a missing browser are skipped.
Run only one verifier per checkout at a time because `npm ci` replaces ignored dependencies.

Linux still needs Chromium's host libraries. CI sets `TRELLIS_BROWSER_SYSTEM_DEPS=1`
so `./tools/verify` runs `install-deps chromium` alongside the build and waits for it
before browser tests. Local setup does not request privileged OS changes.
Playwright 1.64.1 supports Ubuntu 26.04, so CI uses `ubuntu-latest` again.
The macOS CI job only installs the managed Chromium bundles.

## Run the browser test alone

A direct test invocation does **not** run verifier setup. Provision once per checkout with
the same external cache and pinned installer:

```sh
export TRELLIS_RUN_CACHE=/path/to/external/run-cache
mkdir -p "$TRELLIS_RUN_CACHE/npm-cache" "$TRELLIS_RUN_CACHE/playwright-browsers"
npm_config_cache="$TRELLIS_RUN_CACHE/npm-cache" \
  npm ci --prefix tests/browser --ignore-scripts
PLAYWRIGHT_BROWSERS_PATH="$TRELLIS_RUN_CACHE/playwright-browsers" \
  node tests/browser/node_modules/playwright/cli.js install chromium
PLAYWRIGHT_BROWSERS_PATH="$TRELLIS_RUN_CACHE/playwright-browsers" \
  node --test tests/browser/sequence.test.cjs
```

The harness fails if the environment variable, locked package, managed Chromium executable, Cargo build, daemon, or any browser assertion is unavailable. It has no synthetic-DOM fallback and never skips acceptance.

## Pele sequence acceptance

`sequence-pele.test.cjs` serves the exact local browser assets and four producer-validated
DTO snapshots to real Chromium with the same no-inline-style/script CSP. It tests original
note identity, group controls, unsafe text/output, keyboard and stale actions.
`sequence.test.cjs` uses a temporary indexed checkout, the production daemon and
published Store/API response, then selects a real method in the unmodified browser shell.
It checks Pele without fallback, inspector, mouse/keyboard selection, pinned source and
stale actions. `sequence-cost.test.cjs` indexes the 80/180-call Rust cohorts and measures
20 warm old/Pele renders of each *identical published DTO*. No reconstructed DTO is
used for this full-app cohort. All browser files run sequentially after the built binary.

For focused runs, first provision Chromium using the commands above, build with
`cargo build --locked --bin trellis`, then invoke `node --test
--test-concurrency=1 tests/browser/sequence-pele.test.cjs tests/browser/sequence.test.cjs
tests/browser/sequence-cost.test.cjs` with `TRELLIS_SUITE_BINARY` set to the
absolute built binary. Do not run multiple browser suites in parallel against the
same checkout.

### Sequence fixture cleanup self-test

`sequence-harness.test.cjs` checks normal teardown, a thrown test failure, SIGTERM,
and SIGINT without launching a browser:

```sh
TRELLIS_SUITE_BINARY=/absolute/path/to/debug/trellis \
  node --test tests/browser/sequence-harness.test.cjs
```

The missed cleanup path was an interrupted suite: Node does not run `test.after`
on SIGTERM/SIGINT, so its detached, test-elected daemon could outlive the suite.
The harness now handles those signals through the same cleanup owner as normal
teardown. It first stops its CLI/serve children, then stops only detached daemons
whose exact command uses its run-unique suite binary. The self-test waits for an
IPC readiness hook after real daemon election and serve attachment, interrupts
that child suite, and checks both owned PIDs are gone. A daemon using another
suite binary must survive. SIGKILL cannot run JavaScript cleanup. Production
15-minute checkout/browser and 30-minute daemon idle policy is unchanged.
