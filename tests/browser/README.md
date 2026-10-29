# Browser acceptance: diagrams and shell

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

## Pele block-label vendor regression

`pele-block-labels.test.cjs` serves the exact vendored Pele bundle to Chromium and
installs its unchanged raw SVG. It checks label bounding boxes and ordinary mouse
clicks on original node and edge text for vertical, horizontal and repeated
same-pair block edges. A passive listener observes the browser's native target and
stable Pele `data-id`; no wrapper routes clicks or rewrites SVG.
This upstream-only host has no CSP because raw Pele owns inline SVG styles. It is
not evidence about Trellis's CSP; the product adapter tests below retain the strict
no-inline policy. After normal browser provisioning, run it alone with
`node --test tests/browser/pele-block-labels.test.cjs` and the same external
`PLAYWRIGHT_BROWSERS_PATH`. It also runs in `./tools/verify`.

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

## Files acceptance

`files.test.cjs` uses the real daemon and published cached index to exercise Files on desktop
and mobile. It checks the all-symbol outline, indexed source and syntax-match evidence labels,
callable mini-sequence/Open full, and the existing per-file net HEAD-to-working-tree Git diff.
Working-tree fixtures are staged and edited before indexing and startup so assertions do not
race the watcher. Deterministic cached-vs-live divergence belongs to the offline Store/HTTP
regressions, not timing-based browser edits. Git diff coordinates never become indexed
navigation targets. No-Git and repository text remain explicit and safe.

## Source navigation and browser history (#392)

The real-daemon suites use ordinary Java, Rust and JavaScript fixtures:

- `files.test.cjs`: a measured declaration's one-click Sequence uses its exact ID and
  complete pin. Back/Forward restores Files selection, indexed basis, source highlight,
  active line, actual source-panel scroll and focused action without another source read
  or extra browser-history entries. Working-tree diff lines have no indexed actions.
- `sequence.test.cjs`: Read method source opens a different callable than the left-tree
  selection, then Back restores the same source and original selection. Inspector source
  on an unresolved import offers the explicitly labelled enclosing callable, not a guessed
  callee. Call graph stays disabled; no #378 browser view is implied.
- `viewer-shell.test.cjs`: bare and generated pinned `#sequence?sid=…` links wait for
  authentication and explicit checkout selection. The exact returned seed supplies the
  method and source. Missing/unknown IDs and mismatched checkout/generation/revision are
  refused rather than replaced with a successor. Generated links contain checkout and full
  pin, never credentials.
- Libraries uses a local Cargo path dependency, authored before daemon startup. Its real
  immutable candidate source has no workspace Sequence/navigation action, preserves the
  selected workspace diagram, and renders hostile text safely under the real CSP. The
  fixture explicitly requests `POST /dependencies/refresh` before awaiting native catalog
  readiness: a separately investigated startup/serve-registration defect can leave the
  automatic catalog loading without a worker. This setup does not fix or test automatic
  catalog startup. No dependency download, Cargo project execution or positive external
  navigation is claimed.

These are functional assertions, not benchmarks. They observe events or conditions rather
than fixed sleeps. Resolved-callee producer acceptance belongs to the approved backend
projection fixture; this source-navigation coverage does not manufacture resolved targets.

The suite runs in the canonical `./tools/verify` browser lane. For a focused run after normal
setup and build, set `TRELLIS_SUITE_BINARY` to the absolute built binary and run
`node --test --test-concurrency=1 tests/browser/files.test.cjs` with the same external
`PLAYWRIGHT_BROWSERS_PATH`.

## Architecture acceptance

`architecture.test.cjs` uses an isolated real daemon, authenticated checkout selection and
40 indexed Rust file modules. Authored rules exercise ordered and peer bands, shared and
empty layers, and explicit unassigned evidence. Hand-counted `crate::` references include
forward/skipped, reverse, same-layer, cross-peer, shared-outbound and unresolved sites.
Mixed production/test declarations retain stored roles: `test-support` remains production.
The suite compares the browser Architecture DTO to the existing references CLI at the same
complete pin, then checks exact summary counts and UI evidence. No API request is intercepted.
Separate pre-start fixtures cover missing, empty and malformed configuration without watcher races.

The UI checks cover hostile layer text under the real CSP, mouse/Enter/Space selection,
production/test visibility, pinned cached-source actions and narrow-screen containment.
The summary-first display (#391) is covered too: bands start collapsed, "Deviations only" starts
on when a production deviation exists, layers without production edges sit in the quiet footer,
authored `display` hints (`expanded`, `collapsed`, `hidden`) apply, and bands, directories and
footers expand by keyboard with the choice remembered per checkout. Tests that need file chips
expand every band and directory first. `viewer-shell.test.cjs` covers the hidden/ignored file-tree
toggle, the idle status text and the inspector appearing only for a selection.
Module Flow (#324) and the treemap action (#121) remain deferred; no placeholder claims
that navigation works. `architecture-pele.test.cjs` checks pinned-renderer mapping, DOM-order independence,
negative SVG admission and atomic render failure in real Chromium; timing-sensitive pin/rules races use deterministic unit/HTTP
hooks rather than editing a running browser fixture.

After the normal setup and build above, set `TRELLIS_SUITE_BINARY` to the absolute built
binary and use the same external `PLAYWRIGHT_BROWSERS_PATH`, then run:

```sh
node --test --test-concurrency=1 tests/browser/architecture.test.cjs
```

## Treemap acceptance

`treemap.test.cjs` drives the real daemon, indexed per-file line/role/metric response
and exact vendored Pele under the product CSP. It covers independent heat and
test-file controls, native tile/file-list selection, directory navigation, empty
files, pinned source, Architecture entry and stale async actions. Run after normal
provisioning/build with `TRELLIS_SUITE_BINARY` and `PLAYWRIGHT_BROWSERS_PATH`. It is
also part of `./tools/verify`. Area is physical file lines, including inline test
lines, not callable-range LOC. Native adapter admission/mutation tests run in
`tests/treemap-pele.test.cjs`.

## Class acceptance

`classes-pele.test.cjs` serves the exact `web/classes.js` adapter, vendored Pele and
stylesheets under the product CSP, with synthetic class DTOs. It checks native class
node and member identities against the original records, encoded repository text,
fail-closed SVG validation, bounds, the visible card fallback, class menus, members,
method and member navigation by mouse and keyboard, and stale actions.
`classes-app.test.cjs` indexes a Java checkout, opens the class through the real shell
and published daemon response, and follows Read class source and a method member into
its Pele sequence on the same pin. Both run in `./tools/verify`; run them alone after
provisioning with `TRELLIS_SUITE_BINARY` and `PLAYWRIGHT_BROWSERS_PATH`.

## Published semantic source navigation

`navigation-resolved.test.cjs` publishes real native extraction plus validated synthetic
semantic bindings through the hidden test-support-only `navigation-fixture` command.
The exact same executable then runs the daemon, retaining the complete publication pin.
No SQL rows or HTTP responses are fabricated. Cases cover zero, one and multiple
published semantic producers, an unresolved call, and a direct internal callee whose
source spelling deliberately names a different local function. The browser checks the
resolved choice, exact selected seed and Back to the source view.

`./tools/verify` builds and preserves a test-support executable for only this suite;
other browser tests keep the normal binary. For a focused run after browser provisioning,
run `node --test tests/browser/navigation-resolved.test.cjs`. It builds a test-support
binary when `TRELLIS_NAVIGATION_FIXTURE_BINARY` is absent. To avoid that build, point the
variable at a prebuilt test-support `trellis` executable.
