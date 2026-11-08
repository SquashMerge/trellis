# Trellis for Herdr: design notes (#707 foundation)

This first cut is the shell plus one view, Architecture. Owner decisions of 2026-11-08 set the scope:

- the mod mirrors the web app, but has no file list of its own, because files come from a `trellis-files` clone of Herdr's Files panel;
- the first view is Architecture's layer stack;
- Deviations and Apps wait for factory runs 802 and 803.

## What works

| Piece | State | How it was checked |
|---|---|---|
| Build | esbuild bundles `src/main.js`, the browser scripts, Pele and the mod's code into one `main.js`. | `node build.mjs --install`, then `herdr-app check` passes and `herdr-app status` shows no errors. |
| Shell | Checkout selector (the daemon's `/api/checkouts`, with each row's index state), index status (revision, Fresh or Catching up), polling (status every 5 s, list every 30 s, `building` retried at the daemon's `retryAfterMs`). | `test/harness.mjs` runs the real `src/main.js` against a live daemon. |
| Typed errors | Every failure is a code and a message, and the data from that read is dropped. Codes come from the daemon (`index_not_ready`, `unauthorized`, `revision_conflict` and so on) or the mod (`token_missing`, `token_unreadable`, `token_invalid`, `daemon_unreachable`, `invalid_url`, `invalid_response`, and the adapter's `unsupported`). A drawing is shown only for the payload it was made from. | Harness against a stopped port, a non-loopback URL, a missing token and the owner's daemon. Unit tests. |
| Architecture | Fetches the summary with the browser's parameters (`aggregate=max&role=production&workbench=true`, pinned to the status revision) and validates it as the browser's `payloadOf` does. Draws with the browser's own adapter and Pele. Lists the layers natively: order, files, max cognitive, and outgoing counts (↑ ⇄ ↓ →shared and so on, with deviations in the error colour), plus hidden, test, unassigned and coverage lines. | Harness. Unit tests, which use the browser's own Architecture fixture. |
| Clicks | Pressing a band in the diagram, or its row in the list, opens or folds it, as the band header does in the browser. Pressing an arrow's count, a file or a note shows its full label above the drawing. | Harness (`--press L`, `--press E`, `--click layer-web`). |
| Files follow | `trellis-files` publishes `trellis.selectedFile`. The mod switches to the checkout that holds the file. It shows the file's layer, the layer's counts, the file's module chip (name, file count, max cognitive, heat, rank in the band, whether it's drawn or in "+N more"), and the file's own metric. A drawn chip gets an accent outline. | Harness (`--select src/http.rs`, `--select web/architecture.js`). `trellis-files` passes `herdr-app check`. |
| Theme | Every fill and stroke in the SVG is mapped from the browser's brand palette to `theme()` colours. Native text is 13 px body, 12 px secondary and 14 px headings. | A unit test asserts that no unthemed colour remains. |
| Tests | `test/mod.test.mjs` covers the DOM, the measure, drawing, hit-testing, the client and payload checks. It needs no install, and it runs in `tools/verify`'s `checks` lane. | `node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/mod.test.mjs` |

**Not yet seen in the window.** See [SVG fidelity](#svg-fidelity).

## How the diagram is drawn

1. `src/client.js` reads the token, then `/status` for the pin, then `/architecture?…` for the payload.
2. `src/diagram.js` `summarize()` builds the browser's summary-display options with its defaults: metric cognitive, aggregate max, production, fixed bands, tests hidden, bands folded unless `rules.toml` says `expanded`, and the 40/150 chip caps. These are ported from the closure inside `web/architecture.js` `create()`, because that code is DOM-bound and can't be bundled. The ported functions keep their browser names. The pure helpers that file exports (`moduleKey`, `commonDirectory`, `groupModules`) are bundled, not copied.
3. `web/architecture-pele.js` `render()` runs unchanged. It projects to Mermaid block source, fits the width (up to 12 Pele layouts), validates Pele's SVG, rewrites labels, adds stat chips and routes the arrows. It runs over `src/dom.js`, a small SVG DOM with a strict XML parser and the compound selectors the adapter uses. Unsupported selectors throw rather than match wrongly.
4. The vendored Pele (`web/vendor/pele-1.10.0-trellis.2.js`) runs with a shadowed `globalThis`, so it measures with its built-in Helvetica width table. The adapter measures with the same table, which the build extracts from the vendored file, so wrapping and fitting agree with Pele's layout.
5. The tree is serialized with theme colours and written to `~/.config/herdr-app/mods/trellis/.render/architecture-<copy>-<hash>.svg`. The folder is hidden, so the write doesn't reload the mod. The pane shows the file with `image()` at 1:1, and deletes the previous file when a new one is shown, and its own file in `close()`.

The browser scripts are wrapped at build time, by `web-scripts.mjs`, in `export default function (<globals>) {…}`, so the mod never touches the runtime's real `globalThis`. That matters: the runtime has frozen intrinsics. `linkedom` failed to load there with "'constructor' is read-only", which is why the mod has its own small DOM.

In Node, a drawing of the trellis repository (8 layers, 676 modules, 29 edges) takes about 40 ms. The time in the Herdr runtime isn't measured yet. The pane only redraws when the payload, the open bands, the theme, the followed chip or the width (by more than 8 px) changes.

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

**Screenshot: not taken yet.** For the whole session, Herdr's window drew no new frame. `herdr-app shot` returned the same frame each time, so the window is most likely minimized or on another Space. The Trellis pane was opened with `pane.split`, but it never laid out: its `.render/` folder was never created. So nobody has looked at the SVG text in Herdr's renderer yet. That's the main open risk, because the renderer may lack system fonts for SVG `<text>`. To check it, bring the window to the front, open the pane, then run `herdr-app shot /Users/jcarreira/projects/squashmerge/trellis-evidence/707/architecture.png`.

## How clicks on the diagram work, and how they can grow

`draw()` returns every control's box from the finished tree: the band headers (whole band rects), arrow count tags, file and directory chips, notes and the unassigned row. Each box is in SVG units, with Pele's node `translate()` applied. The image is drawn at 1:1, so `on_mouse_down`'s `local_position` (element-relative, logical px) is already in SVG units. `hit()` picks the smallest box under the pointer.

Next steps, in increasing cost:

- **Selection highlight.** Re-serialize the same tree with an override for the pressed node, without a new layout. The followed-file outline already does this.
- **Hover detail.** `on_mouse_move` has `local_position` too. Show the hovered box's label in a strip, with no redraw of the image.
- **Zoom or fit-to-width.** Keep a scale factor and divide `local_position` by it.
- **Keyboard.** Set `"focusable": true` in `mod.json`, then cycle through `boxes` in drawing order and draw a focus outline the same way as the selection highlight.
- **Inspector.** On a pressed arrow or band, read `/api/architecture/details` (paged, pinned to the same revision and rules hash, as the browser does) and list the pairs natively.

## Bundle size

`main.js` is **839.7 KiB**. esbuild minifies whitespace and syntax, but keeps names for readable stack traces.

| Part | Size |
|---|---|
| Vendored Pele (already minified) | 695.5 KiB |
| `web/architecture.js` (only three helpers are used) | 61.2 KiB |
| `web/architecture-pele.js` | 38.9 KiB |
| Mod code | 40.4 KiB |
| `web/metric-heat.js` | 2.6 KiB |

`herdr-app check` loads and renders it in about 0.15 s. If size matters later, the easiest saving is to move `moduleKey`, `commonDirectory` and `groupModules` into their own browser script, which saves about 60 KiB.

## Auth

- The mod reads the serve token file named in its props, by default `~/.config/trellis/serve.token`. That is the file `trellis serve --token-file` loads or creates with mode 600, and the file the web app's "Load token file" reads.
- The token stays in the client closure. It's sent only as `Authorization: Bearer` to the configured loopback URL. It's never logged, stored in mod state, `localStorage` or shared state, written to disk, or put in an error message. A unit test checks the messages.
- A 401 or 403 drops the token, so it's re-read from the file on the next request, after `serve` restarts with a new file.
- Only `http://127.0.0.1:<port>`, `http://localhost:<port>` and `http://[::1]:<port>` are accepted.
- The daemon's guard accepts the request because the mod's `fetch` sends a loopback `Host` and no `Origin`.

**Reading a checkout's status starts indexing it.** So the mod never picks a checkout by itself. It opens one only if:

- it was chosen before, or named in props;
- it holds the focused terminal's folder;
- it holds the file selected in `trellis-files`.

## The `trellis-files` clone

`trellis-files/publish-selection.patch` is the whole diff from the clone that `mod.clone` makes:

- it imports `shared` and `setShared`;
- it adds a `publish()` helper;
- it adds one call in `show()`.

`show()` is where the Files panel opens a file and where it clears one. The patch publishes `{root, path, on}` and only writes when the value changes. `on` (a remote machine) is published, but the Trellis pane ignores remote files.

## What each remaining view needs

- **Classes.** It needs `GET classes` (the catalog), `POST class-diagram` and `web/classes.js`'s Pele adapter. That adapter also parses with `DOMParser` and uses compound and `:scope >` selectors, so it should run on `src/dom.js`. Its DOM-bound UI (members toggle, chooser) would be ported to native rows, as Architecture's options were. Clicks would use the same box hit-testing, on class and member nodes.
- **Sequence.** It needs `POST sequence` (`{seed, expectedRevision, showAll, interactions}`) with a method chosen from `GET methods` or the followed file's symbols (`GET symbols?path=`). `web/sequence-pele.js` is the adapter, so check its selectors against `src/dom.js`. The browser's step list and source dock become native rows. Source reads (`GET source`, revision-pinned) can use the shipped `Editor` in read-only mode.
- **Treemap and metrics.** It needs `GET metrics` and `web/treemap-pele.js`, which uses the same DOM pattern plus `<title>`, which `src/dom.js` handles. `web/viewer-metrics.js` holds the metric-panel logic. A followed file maps to its treemap box directly.
- **Search.** The daemon has no browser search route today; search is MCP-only (`trellis_search_symbols` and `trellis_find_symbols`). This view needs either a new authenticated loopback route or `exec("trellis", …)` against the CLI. Results are native rows, and a click opens the file in `trellis-files` through shared state, the reverse of the current publish.
- **Deviations.** It waits for factory runs 802 and 803, which are changing the Architecture API. It will then be a native tree over the new rule-evidence payload, paged through `architecture/details`.
- **Apps.** It waits for the same two runs, and for the apps and per-app layers in `rules.toml` (#783). It's probably the Architecture drawing scoped to one app, with an app chooser in the header.

## Open problems

1. **Visual check.** See [SVG fidelity](#svg-fidelity).
2. **The owner's daemon.** Its checkouts report `unavailable · root_changed` or `index_not_ready` ("Storage is unavailable"). The pane shows that as a typed state. A diagram was verified only against an isolated daemon: `HOME=<scratch> trellis serve --bind 127.0.0.1:8878` on this worktree.
3. **No request timeout.** The runtime's `fetch` takes no signal or timeout. A hung daemon leaves "Reading…" until the next poll.
4. **Ported summary options.** They must follow `web/architecture.js`. The real fix is for the web app to export them as a pure function both sides bundle. That edit touches `web/architecture.js`, which runs 802 and 803 are changing, so it waits.
5. **No drilled-in directories.** Only band open and fold is supported, so a directory module stays one chip.
