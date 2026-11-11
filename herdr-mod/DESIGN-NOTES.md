# Trellis for Herdr: design notes (#707 foundation)

This cut is the shell plus two views, Architecture and Sequence. The daemon can be on another Herdr machine (round 2, 2026-11-08). The pane follows the focused agent's checkout (#809 item 1, 2026-11-08); agents per checkout (#809 item 2) is not built. Symbol follow (#809 item 3) is replaced by the agent's explicit `trellis_show` (#818). Owner decisions of 2026-11-08 set the scope:

- the mod mirrors the web app, but has no file list of its own, because files come from a `trellis-files` clone of Herdr's Files panel;
- the first view is Architecture's layer stack;
- Deviations and Apps wait for factory runs 802 and 803.

## What works

| Piece | State | How it was checked |
|---|---|---|
| Build | esbuild bundles `src/main.js`, the browser scripts, Pele and the mod's code into one `main.js`. | `node build.mjs --install`, then `herdr-app check` passes and `herdr-app status` shows no errors. |
| Follow | The pane shows the checkout the focused agent or terminal works in, or the Files selection's, whichever is more recent. A pin stops agent follow. See [Which checkout is shown](#which-checkout-is-shown). | `view.test.mjs`: agent A then B, a mod-pane focus, pinned, a later Files selection, an unknown folder. Harness with `--cwd`. |
| Agent shows (#818) | Each status poll carries the checkout's `viewRequest`. The first one seen for a checkout is a baseline; a newer generation opens that Sequence (read at the pane's pin, checked against the seed as the browser checks it) with a "Shown by agent" note and Back, unless the pane is pinned. A pinned pane counts the request as seen, so unpinning does not replay it. A request at another snapshot, or for Classes or source, which the pane lacks, is a note only. Focus never moves. | `view.test.mjs`: shown with Back, baseline, equal and older generations, pinned then unpinned, another snapshot, class and source notes. |
| Shell | The followed checkout's label and pin (the daemon's `/api/checkouts`, with each row's index state), index status (revision, Fresh or Catching up), polling (status every 5 s, list every 30 s, `building` retried at the daemon's `retryAfterMs`). | `test/harness.mjs` runs the real `src/main.js` against a live daemon. |
| Stale reads | Every async read (checkouts, status, Architecture and its building retries, methods, sequence, both drawings) owns a token for its one request. A result, rejection or `finally` of a request that is no longer current changes nothing, not even a busy flag a successor owns, and a checkout switch starts its own status read (#819 review). | `view.test.mjs`, with deferred promises: a status read obsoleted by a switch, a sequence answered out of order, an older revision's Architecture failing after the newer one succeeded, an older checkout list, an older method list. |
| Typed errors | Every failure is a code and a message, and the data from that read is dropped. Codes come from the daemon (`index_not_ready`, `unauthorized`, `revision_conflict` and so on), from `trellis api` (`daemon_unavailable`, `route_not_allowed`, `daemon_outdated` and so on) or from the mod (`token_missing`, `token_unreadable`, `token_invalid`, `daemon_unreachable`, `daemon_timeout`, `trellis_missing`, `trellis_outdated`, `machine_unreachable`, `invalid_url`, `invalid_response`, and the adapter's `unsupported`). A drawing is shown only for the payload it was made from. | Harness against a stopped port, a non-loopback URL, a missing token and the owner's daemon. Unit tests. |
| Architecture | Fetches the summary with the browser's parameters (`aggregate=max&role=production&workbench=true`, pinned to the status revision) and validates it as the browser's `payloadOf` does. Draws with the browser's own adapter and Pele. Lists the layers natively: order, files, max cognitive, and outgoing counts (↑ ⇄ ↓ →shared and so on, with deviations in the error colour), plus hidden, test, unassigned and coverage lines. | Harness. Unit tests, which use the browser's own Architecture fixture. |
| Transport | A space on this computer uses `fetch`. A space on another Herdr machine runs `trellis api` there through `exec` (#817). See [Transport](#transport). | `view.test.mjs` drives the real pane through a stand-in `trellis api` on a named machine, and checks a daemon error, an unreachable daemon, an exec timeout, no `trellis` and an old `trellis`. The Rust tests in `tests/daemon/daemon_cli.rs` cover the CLI itself. The harness ran through the real CLI against an isolated daemon, including a 1.5 MB Architecture answer (`trellis-evidence/707/trellis-api-isolated-e2e.txt`). |
| Sequence | The Files selection lists the file's methods (`GET methods`). The first consequential named method opens (`POST sequence`, checked against the pin and seed as the browser checks it). The browser's own `web/sequence-pele.js` draws it. The steps are native rows. Clicking a proven call (diagram or row) re-centers on its target, with Back. Groups expand and collapse. | `view.test.mjs` re-centers on a resolved call and goes back. Harness against the isolated daemon, which is syntax-only, so it has no proven targets. |
| Clicks | Pressing a band in the diagram, or its row in the list, opens or folds it, as the band header does in the browser. Pressing an arrow's count, a file or a note shows its full label above the drawing. | Harness (`--press L`, `--press E`, `--click layer-web`). |
| Files follow | `trellis-files` publishes `trellis.selectedFile`. The mod switches to the checkout that holds the file. It shows the file's layer, the layer's counts, the file's module chip (name, file count, max cognitive, heat, rank in the band, whether it's drawn or in "+N more"), and the file's own metric. A drawn chip gets an accent outline. | Harness (`--select src/http.rs`, `--select web/architecture.js`). `trellis-files` passes `herdr-app check`. |
| Theme | Every fill and stroke in the SVG is mapped from the browser's brand palette to `theme()` colours. Native text is 13 px body, 12 px secondary and 14 px headings. | A unit test asserts that no unthemed colour remains. |
| Drawing in the app | Both drawings yield to the host between phases, and a watchdog turns a drawing that never finishes into a typed `draw_timeout`. See [The synchronous-run cap](#the-synchronous-run-cap). | In the live app (2026-11-08), against the owner's daemon: Architecture of the trellis checkout drew, and so did Sequence of `src/daemon/mod.rs` (with the Files selection stood in for; see [Open problems](#open-problems)). Screenshots are in `trellis-evidence/707/`. Unit tests check that yielding changes no byte of either drawing; `view.test.mjs` checks the watchdog in both views. |
| Tests | `test/mod.test.mjs` covers the DOM, the measure, drawing, hit-testing, the client and payload checks. `test/view.test.mjs` drives the pane. They need no install, and they run in `tools/verify`'s `checks` lane. | `node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/mod.test.mjs herdr-mod/test/view.test.mjs` |

## Parity with the web app's Architecture view (#886)

The mod now draws Architecture from the browser's own modules, so the two can't drift:

- **Summary display.** `web/architecture.js` exports `createDisplay(state)`: the band labels and stat chips, the module chips and their budget, the per-app parts of All apps, the heat scale, the legend words and the review counts. The browser's `create()` and the mod both call it. The mod's earlier port of these functions is gone.
- **Apps.** `web/architecture-apps.js` (pure) gives the switcher's choices, the source line, the All apps column layout, column headers, interface cards, cross-app lines and the inspector facts (`appFacts`, `interfaceFacts`, `footnote`, new and used by the browser too).
- **Deviations.** The Deviations tree is `web/architecture-deviations.js`'s pure `model`, over pages of `architecture/details`, shown as native rows.
- **Colours and controls.** `web-scripts.mjs` reads the `--sm-*` tokens from `web/style.css` and the Color by and Aggregation options (and their default) from `web/index.html` at build time. Heat fills, the error tone and ink on heat keep the web colours; surfaces, text and lines use the Herdr theme.
- **Logo and status line.** The header shows `web/brand/trellis-mark-transparent-v1.png` (copied as `trellis-mark.png` by `build.mjs`) and "Trellis by SquashMerge". The status line adds the web app's "Last re-index failed · code: reason" from `jobs/current`.
- **`trellis api`.** It now also serves `GET …/architecture/details` and `GET …/jobs/current`, so the remote transport has the same features.

Choices made here, for the owner to confirm:

1. **Inspector below the drawing.** The pane is too narrow for the browser's side column, so "Architecture review" (counted review chips, then the selected app, interface or control) sits under the legend.
2. **All apps cards.** Side by side, the browser floats each interface card in the gutter between its columns; the pane lists the cards under the drawing. Stacked (below about 904 px of pane), a card follows its client's column, as in the browser.
3. **Deviations tree scope.** File, function and line rows with the edge chips, read-only. Closure naming (the Files outline), Calls only, line menus, exceptions and Open in Sequence stay in the browser. The interface inspector lists counts, not the endpoint lists.
4. **Treemap and Classes are not included.** Each needs a new route through `trellis api`, a new adapter run on `src/dom.js` and native controls; reuse doesn't make them nearly free.

## How the diagram is drawn

1. `src/client.js` reads the token, then `/status` for the pin, then `/architecture?…` for the payload.
2. `src/diagram.js` `summarize()` calls the browser's own `createDisplay` (`web/architecture.js`, #886) with the pane's choices: Color by and Aggregation, the app, open bands and directories. Production role, fixed bands and hidden tests are the browser's defaults.
3. `web/architecture-pele.js` `render()` runs unchanged. It projects to Mermaid block source, fits the width (up to 12 Pele layouts), validates Pele's SVG, rewrites labels, adds stat chips and routes the arrows. It runs over `src/dom.js`, a small SVG DOM with a strict XML parser and the compound selectors the adapter uses. Unsupported selectors throw rather than match wrongly.
4. The vendored Pele (`web/vendor/pele-1.10.0-trellis.2.js`) runs with a shadowed `globalThis`, so it measures with its built-in Helvetica width table. The adapter measures with the same table, which the build extracts from the vendored file, so wrapping and fitting agree with Pele's layout.
5. The tree is serialized with theme colours and written to `~/.config/herdr-app/mods/trellis/.render/architecture-<copy>-<hash>.svg`. The folder is hidden, so the write doesn't reload the mod. The pane shows the file with `image()` at 1:1, and deletes the previous file when a new one is shown, and its own file in `close()`.

The browser scripts are wrapped at build time, by `web-scripts.mjs`, in `export default function (<globals>) {…}`, so the mod never touches the runtime's real `globalThis`. That matters: the runtime has frozen intrinsics. `linkedom` failed to load there with "'constructor' is read-only", which is why the mod has its own small DOM.

In Node, a drawing of the trellis repository (8 layers, 676 modules, 29 edges) takes about 40 ms. In the Herdr runtime one `project()` takes 31–42 ms, and the whole drawing about 0.5 s, so it has to yield (next section). The pane only redraws when the payload, the open bands, the theme, the followed chip or the width (by more than 8 px) changes.

## The synchronous-run cap

**The Herdr runtime silently ends a mod's task when one synchronous run takes too long.** It raises no error, rejects no promise, runs no `finally`, and writes nothing to `herdr-app status`. The app stays idle, and later renders and tasks carry on. Herdr doesn't document it. The pane used to sit on "Drawing the layer stack…" for ever because of it.

Measured on 2026-11-08 with a timed probe in the live app (Herdr app on this Mac mini; the probe logged to a file under `~/.config/herdr-app/` and was then deleted). A "run" is the code between two awaits. Each case below is a busy loop of that length:

| Where the run is | Survived | Ended | Cap |
|---|---|---|---|
| The first run of a `cx.spawn` body started from `render()` (the body runs at once, inside the render call) | 50 ms (twice) | 55 ms (twice) | about 50 ms |
| A `cx.spawn` body started from a timer, or any run after an await | 450 ms (twice) | 520 ms (twice) | about 500 ms |

What resets the budget: **any await**, including `await Promise.resolve()`. Ten 40 ms runs with a microtask await between them finished, 400 ms in all, even from a task started in `render()`. `cx.sleep(0)` also resets it and takes 0–4 ms. So the cap is per run, not per task.

The pane starts its drawings from `render()`, which is why they died: the whole `draw()` was one run inside the 50 ms render cap. The fix:

1. **Leave `render()` first.** Each drawing task awaits `cx.sleep(0)` before any work.
2. **Yield between phases.** `web/architecture-pele.js` and `web/sequence-pele.js` take an optional `options.pause`, an async function awaited between their heavy phases: Architecture's channel reserve, each fit attempt's `project()` and `pele.render`, the re-projection, `validatedSvg`, the label and control pass and routing; Sequence's projection, Pele, `validatedSvg` and the row pass. When `pause` is absent, nothing new is awaited, so the browser behaves as before. The mod's `draw()` and `drawSequence()` also yield around `serialize()`. The mod passes `() => cx.sleep(0)`, a host await that also lets the window draw a frame. A resolved promise would reset the cap too, but wouldn't let anything else run.
3. **A watchdog.** A drawing that hasn't finished after 15 s becomes a typed `draw_timeout` error, and the drawing slot is freed. A task the runtime ended never runs its `finally`, so without it the pane would wait for ever. The same inputs aren't retried; new data, a new width or a theme change try again.

Slices measured in the app on the trellis checkout, at the pane's width (ms, in order):

- Architecture: `1, 33, 32, 2, 32, 2, 5, 2, 9, 4, 0`. That's the summary, the channel-reserve projection, then two fit attempts of `project()` and Pele, then `validatedSvg`, the label pass, routing, `serialize()` and the boxes. The slowest is `project()` at 31–42 ms over several runs. That's within the 40 ms target, but only just. It is far below the roughly 500 ms cap that applies after the first yield. Splitting `project()` itself would mean making the projection async, which isn't needed now.
- Sequence (`src/daemon/mod.rs`, `new`): `0, 1, 2, 6, 0, 2, 0`.

## SVG fidelity

The layout comes from the same adapter code, the same patched Pele build and the same projection and routing as the browser. So the bands, chip rows, "+N more" chips, stat chips, arrow lanes, count tags and heat order match the browser for the same payload and width. They differ in four ways:

1. **Font.** The browser draws in Inter and measures it with a canvas. The mod measures with Pele's Helvetica table and draws in `Helvetica, Arial, sans-serif`, which is installed on macOS. Inter isn't installed system-wide, and Herdr's SVG renderer can't use the web app's `woff2`. Chips and wraps come out a few percent narrower than in the browser.
2. **Colours.** The brand palette maps to theme names:

   | Brand | Theme |
   |---|---|
   | ink | `background` |
   | white | `foreground` |
   | muted | `muted` |
   | control | `input` |
   | surface | `surface` |
   | raised | `raised` |
   | line | `border` |
   | error | `ansi1` |
   | gold, amber, orange | `ansi11`, `ansi3`, `ansi9` |

   Ink text on heat fills becomes `ansi0`, so it stays dark in light themes.
3. **No hover, focus or pressed states.** The image is static.
4. **The browser's resize fit uses the canvas's inner width.** The mod uses the pane width less 24 px of padding.

**Screenshots** (2026-11-08) are in `/Users/jcarreira/projects/squashmerge/trellis-evidence/707/`: `architecture-in-app-final.png` and `sequence-in-app-daemon-mod-rs.png`. Herdr's renderer draws the SVG `<text>` in the system Helvetica.

## How clicks on the diagram work, and how they can grow

`draw()` returns every control's box from the finished tree: the band headers (whole band rects), arrow count tags, file and directory chips, notes and the unassigned row. Each box is in SVG units, with Pele's node `translate()` applied. The image is drawn at 1:1, so `on_mouse_down`'s `local_position` (element-relative, logical px) is already in SVG units. `hit()` picks the smallest box under the pointer.

Next steps, in increasing cost:

- **Selection highlight.** Re-serialize the same tree with an override for the pressed node, without a new layout. The followed-file outline already does this.
- **Hover detail.** `on_mouse_move` has `local_position` too. Show the hovered box's label in a strip, with no redraw of the image.
- **Zoom or fit-to-width.** Keep a scale factor and divide `local_position` by it.
- **Keyboard.** Set `"focusable": true` in `mod.json`, then cycle through `boxes` in drawing order and draw a focus outline the same way as the selection highlight.
- **Inspector.** On a pressed arrow or band, read `/api/architecture/details` (paged, pinned to the same revision and rules hash, as the browser does) and list the pairs natively.

## Bundle size

`main.js` is **910.6 KiB** with Sequence (it was 839.7 KiB with Architecture only). esbuild minifies whitespace and syntax, but keeps names for readable stack traces.

| Part | Size |
|---|---|
| Vendored Pele (already minified) | 695.5 KiB |
| `web/architecture.js` (only three helpers are used) | 61.2 KiB |
| `web/architecture-pele.js` | 38.9 KiB |
| `web/sequence.js` and `web/sequence-pele.js` | 55.3 KiB |
| Mod code | 57.1 KiB |
| `web/metric-heat.js` | 2.6 KiB |

`herdr-app check` loads and renders it in about 0.15 s. If size matters later, the easiest saving is to move `moduleKey`, `commonDirectory` and `groupModules` into their own browser script, which saves about 60 KiB.

## Transport

Mods run in the app that draws them. When the owner views the mini from the laptop, the mod runs on the laptop, and a `fetch` to `127.0.0.1` would reach the laptop. Herdr has no remote `fetch` and no port forwarding. So the pane picks the machine from the shown space (`spaces()[i].machine`, `null` for this computer), unless props name one. A machine Herdr marks `unavailable` in `machines()` is a typed `machine_unavailable` state.

- **This computer:** `fetch(url)`, with the token read from the token file into the client closure.
- **Another machine (#817):** each request is one `exec("trellis", ["api", METHOD, target, ("--body", JSON)], { on: machine, timeout: 30000 })`. `trellis api` is a bounded CLI passthrough to exactly the routes this mod uses (checkouts, status, Architecture, methods, sequence). It reaches that machine's daemon over the private per-user socket, the way `trellis status` does, so no token is read by the mod or by any shell, and `trellis serve` needn't be running there. Its contract is in [`docs/daemon-v1.md`](../docs/daemon-v1.md#trellis-api-browser-routes-through-the-cli-817). The pane reads its stdout and exit code:

  | Outcome | Code |
  |---|---|
  | Exit 0 | the daemon's answer |
  | Exit 1 | the daemon's own `{error}` code, unchanged |
  | Exit 2 | the CLI's typed code: `daemon_unavailable`, `deadline_exceeded`, `route_not_allowed`, `daemon_outdated` and so on |
  | Exit 127 | `trellis_missing` |
  | No JSON, and clap's "unrecognized subcommand" | `trellis_outdated` |
  | `exec` rejected with a timeout | `daemon_timeout` |
  | `exec` rejected otherwise | `machine_unreachable` |

  Answers are bounded by the CLI's 16 MiB and exec's 8 MiB. The CLI gives up on a silent daemon after 25 s, before exec's 30 s. A POST body (sequence seed and pin) is in the command's argv; it holds no secret.
- **Why `fetch` stays for this computer.** The brief allowed either. `fetch` works with a daemon started before `trellis api` existed, so the owner's pane on the mini keeps working until the new binary is installed and the daemon restarts. It also spawns no process per poll. Its one gap is the missing request timeout (the runtime's `fetch` takes none); `transport: "exec"` uses the CLI here too, with its 25 s bound, and needs no token. Once every machine runs a `trellis` with `api`, switching this computer to the CLI as well would remove the token handling from the mod entirely; it would be equally bounded.
- **The token, on this computer only.** It is never in the viewing app's logs, mod state or any `exec` argument. On another machine there is no token in the mod's path at all.
- **Polling:** status every 5 s locally, and every 10 s for another machine, because each poll runs a command there.
- **Files-panel paths:** `trellis-files` publishes `on` (its space's machine). The pane follows only a file on the machine it shows, so a remote file maps to that machine's checkouts.

## Auth

- The mod reads the serve token file named in its props, by default `~/.config/trellis/serve.token`. That is the file `trellis serve --token-file` loads or creates with mode 600, and the file the web app's "Load token file" reads.
- The token stays in the client closure. It's sent only as `Authorization: Bearer` to the configured loopback URL. It's never logged, stored in mod state, `localStorage` or shared state, written to disk, or put in an error message. A unit test checks the messages.
- A 401 or 403 drops the token, so it's re-read from the file on the next request, after `serve` restarts with a new file.
- Only `http://127.0.0.1:<port>`, `http://localhost:<port>` and `http://[::1]:<port>` are accepted.
- The daemon's guard accepts the request because the mod's `fetch` sends a loopback `Host` and no `Origin`.

**Reading a checkout's status starts indexing it.** So the mod never picks a checkout by itself. It opens one only if:

- it holds the focused agent's or terminal's folder;
- it holds the file selected in `trellis-files`;
- it was shown last time, or named in props, and nothing is focused or selected yet.

## Which checkout is shown

The owner's request (#809 item 1, 2026-11-08): the pane goes with the current agent, not a dropdown.

- **Two events.** Each render reads `focused()` and shared `trellis.selectedFile`. A change of the focused pane or its `cwd` with a `cwd` (an agent or terminal) is an agent event; a new Files value is a selection event. Each gets the next sequence number, and the more recent one decides.
- **Mod panes.** A mod pane's `cwd` is null. Focusing one, this pane included, changes the focus key but records no agent event, so the last agent-or-terminal folder stays.
- **Mapping.** The folder maps to the deepest checkout in the daemon's list whose `workspaceRoot` is it or holds it, so a worktree inside another checkout wins. A folder in none is a note naming it, and nothing is read. A listed checkout that isn't `available` is a note too.
- **Pin.** Pinned ignores agent events. Only a selection made after pinning switches. Unpinning counts the current focus as a new event, so the pane catches up at once. The pin and the last checkout are kept in `localStorage`, a per-viewer convenience.
- **The list refreshes** every 30 s, and each refresh re-runs the mapping, so a fresh clone appears once the daemon lists it.

## The `trellis-files` clone

`trellis-files/publish-selection.patch` is the whole diff from the clone that `mod.clone` makes:

- it imports `shared` and `setShared`;
- it adds a `publish()` helper;
- it adds one call in `show()`.

`show()` is where the Files panel opens a file and where it clears one. The patch publishes `{root, path, on}` and only writes when the value changes. `on` (a remote machine) is published, but the Trellis pane ignores remote files.

## Reaching trellis-files

The `trellis` layout (`layout/trellis.json`) is a clone of Herdr's `main`. Its side panel `panel` is a row: the stack `panel-body`, which shows `files` (`{mod: "trellis-files"}`) or `changes`, and the `trellis` pane beside it. The shipped `tabs` mod opens that panel. Its Files and Changes buttons run `layout.select {stack: "panel-body", child}` and `layout.show {id: "panel"}`, and it draws a button only for the names in shared `"workspace.panels"` (both when unset). The welcome can set that to `["changes"]`, as it did on the owner's laptop. Then nothing ever selects `files`, and `trellis-files` can't be reached: Changes shows the changes and the Trellis pane, and nothing else.

**Choice (2026-11-08): the Trellis pane adds `"files"` to `"workspace.panels"`** (`src/panels.js`, called from `init()`), with `setShared`, the supported way for a mod to set shared state. It does so only when the shown layout's `panel-body` has a `files` child whose mod is `trellis-files`, and only when the value is set and lacks `"files"`. It writes nothing otherwise, so a second start changes nothing, and a Trellis pane opened in another layout leaves the value alone. The tab bar then shows Files and Changes, exactly as in Herdr's default.

Why this, rather than restructuring the layout:

- **It uses the tab bar as designed.** `"workspace.panels"` is the documented switch for exactly these buttons, and the layout keeps `main`'s ids and shape, which the `tabs` mod finds them by. Files opens `trellis-files` with the Trellis pane; Changes opens the changes with it.
- **The layout alternatives were worse.** As long as Changes is the only button, the only state it can reach is the panel shown on `changes`. So `trellis-files` would have to sit outside the stack: either always beside the changes (three columns whenever the panel opens), or in a second, always-shown column that no button toggles. A stack `"header": {"content": "tabs"}` (the documented built-in strip) parses in this Herdr build, but draws no strip, so it can't switch the stack either.
- **No hand edit of `shared.json`.** There is no app command that sets shared state, so the setting has to come from a mod. The Trellis pane is in the layout that needs it.

The cost: the value is shared, so `main` also shows a Files button (the shipped Files panel), as it does by default. The pane starts the first time the panel shows, so on a fresh install the Files button appears once the panel has been opened (Changes) or once `setup-viewer.sh` has run, which opens it.

Checked on the mini (2026-11-08): reinstalling the mod reloaded the pane, and `"workspace.panels"` went from `["changes"]` to `["files", "changes"]`. `herdr-app status` shows layout `trellis`, no errors. The in-app screenshot couldn't be taken: the mini's screen was locked, so the window drew no new frames and `herdr-app shot` returned a stale frame. The state is recorded in `trellis-evidence/707/viewer-files-reachable-state.txt`. A unit test covers the rule against `layout/trellis.json`.

## What each remaining view needs

- **Classes.** It needs `GET classes` (the catalog), `POST class-diagram` and `web/classes.js`'s Pele adapter. That adapter also parses with `DOMParser` and uses compound and `:scope >` selectors, so it should run on `src/dom.js`. Its DOM-bound UI (members toggle, chooser) would be ported to native rows, as Architecture's options were. Clicks would use the same box hit-testing, on class and member nodes.
- **Sequence: built** (`src/sequence.js`). `web/sequence-pele.js` runs unchanged, over the helpers in `web/sequence.js`, which is wrapped unchanged; only its exported helpers are used, not its own SVG renderer. Its HTML expand/collapse buttons are `foreignObject`s, which an image can't draw, so they're left out of the SVG; the step rows expand and collapse groups instead. Pele's `var(--_x)` theme slots are filled from `theme()`, as the browser's stylesheet fills them, and the selected step is filled with `selected`. Not yet: the source dock (`GET source` into the shipped read-only `Editor`), Show all steps, the inspector, and the browser's fallback renderer when Pele refuses a view. That case is a typed `unsupported` error above the native step list, which still holds the whole answer.
- **Treemap and metrics.** It needs `GET metrics` and `web/treemap-pele.js`, which uses the same DOM pattern plus `<title>`, which `src/dom.js` handles. `web/viewer-metrics.js` holds the metric-panel logic. A followed file maps to its treemap box directly.
- **Search.** The daemon has no browser search route today; search is MCP-only (`trellis_search_symbols` and `trellis_find_symbols`). This view needs either a new authenticated loopback route or `exec("trellis", …)` against the CLI. Results are native rows, and a click opens the file in `trellis-files` through shared state, the reverse of the current publish.
- **Deviations and Apps: built** (#886; see [Parity](#parity-with-the-web-apps-architecture-view-886)).

## Open problems

1. **The synchronous-run cap** is undocumented. The numbers above are measurements, and Herdr could change them. A drawing that grows past it shows `draw_timeout` instead of hanging. Feedback asking Herdr to document the cap and report it was drafted for the owner on 2026-11-08.
2. **No proven call targets on syntax-only indexes.** Re-centering follows the browser's proof rule (resolved, direct, fresh evidence). The isolated daemon has no semantic producer, so its views have no proven targets; it is tested with a resolved fixture.
3. **Driving the Files panel from a script.** Nothing but a real click sets `trellis.selectedFile`. Posted mouse events need Accessibility permission, which the agent's shell doesn't have. So the in-app Sequence check used a temporary build that stood in for the selection (`src/daemon/mod.rs`) and opened the Sequence tab. The selection path itself is covered by `view.test.mjs` and the harness (`--select src/daemon/mod.rs --click tab-sequence`, against the owner's daemon on 8877).
4. **No request timeout on this computer.** The remote path has the CLI's 25 s bound and exec's 30 s. Local `fetch` has none; `transport: "exec"` is the workaround.
5. **Screenshots of #886** were taken from the pane's real render tree laid out as HTML (`trellis-evidence/886/render/`), not from a Herdr window, because taking one would mean installing into the owner's live Herdr. After `node build.mjs --install`, confirm with `herdr-app shot`.
