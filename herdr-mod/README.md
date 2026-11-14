# Trellis for Herdr

A [Herdr app](https://herdr.dev) mod that shows Trellis in a pane beside the coding agents (#707). It has three tabs, drawn the same way the web app draws them:

- **Architecture:** the web app's Architecture view (#886): Color by and Aggregation, the app switcher with one column per app, HTTP interface cards and cross-app lines, the deviation counts, the heat legend and the inspector, with the layers and their counts as native rows.
- **Sequence:** a method's static call sequence, with its steps as native rows. Clicking a proven call re-centers on that method.
- **Deviations:** the Deviations tree (#939) with its edge chips. The review chip, an app's deviation count and a deviating arrow (its label, line or head) open it with their filter. A line opens its caller in Sequence, or shows its location when it has none (this pane has no source view).

Both views follow the file selected in the `trellis-files` panel. The pane talks to the Trellis daemon on the machine the shown space lives on.

- `src/main.js` is the pane: the followed checkout with a pin, index status, typed error states, the diagram and the layer list.
- `src/client.js` is the daemon client: routes, the two transports (`fetch` here, `trellis api` elsewhere) and typed errors.
- `src/diagram.js` draws Architecture. It uses the browser's own `web/architecture-pele.js`, the `web/pele-svg.js` it reads, `web/metric-heat.js`, `web/architecture.js` (`createDisplay`), `web/architecture-apps.js`, `web/architecture-deviations.js` (its model) and the vendored Pele, bundled unchanged, and reads the colour tokens and the Color by choices from `web/style.css` and `web/index.html` at build time.
- `src/architecture-ui.js` lays out the Architecture tab's native parts: controls, switcher, column headers, interface cards, legend, inspector and Deviations tree.
- `src/sequence.js` draws Sequence. It uses `web/sequence-pele.js` and the helpers in `web/sequence.js`, with the `web/source-witness.js` they both read and the `web/pele-svg.js` the adapter reads, bundled unchanged.
- `src/dom.js` is the small SVG DOM the adapter needs, because the mod runtime has none.
- `trellis-files/publish-selection.patch` is the one change to a clone of Herdr's shipped Files mod.
- `src/panels.js` turns on the tab bar's Files button in the `trellis` layout.
- `layout/trellis.json` is the `trellis` layout, and `setup-viewer.sh` sets up a viewing laptop.

See [DESIGN-NOTES.md](DESIGN-NOTES.md) for how it works, what's missing and what comes next.

## Requirements

- The Herdr app, with mods in `~/.config/herdr-app/mods/` (or `$HERDR_APP_HOME/mods/`).
- Node 24 and npm, to build.
- On this computer, a Trellis daemon listening on loopback. The mod talks to `http://127.0.0.1:8877` with the token in `~/.config/trellis/serve.token`, the file `trellis serve` was started with:

  ```sh
  trellis serve --token-file ~/.config/trellis/serve.token
  ```
- On another Herdr machine (a mini viewed from a laptop), a `trellis` with the `api` command on that machine's `PATH`. Its daemon is reached through the CLI, so `trellis serve` and the token aren't needed there. A daemon started before `trellis api` existed answers `daemon_outdated`: restart it (`trellis daemon stop`, then start `trellis serve` again).

## Install on a laptop that views this Mac mini

Mods run in the app that draws them. A laptop's Herdr app runs the mods in the laptop's own `~/.config/herdr-app/mods/`, so the mods and the layout have to be copied there. They still talk to the mini's daemon, by running `trellis api` on the mini through `exec`.

The mini (below) builds and installs both mods, the `trellis` layout and `setup-viewer.sh`. On the laptop, run this one line. `MINI` is the ssh host the laptop's Herdr uses for the mini, the name shown under Machines:

```sh
ssh MINI 'cat ~/.config/herdr-app/mods/trellis/setup-viewer.sh' | sh -s MINI
```

`setup-viewer.sh` (POSIX sh, safe to run again):

1. copies `mods/trellis`, `mods/trellis-files` and `layouts/trellis.json` from the mini in one `tar` over ssh, leaving out `.render`, `*.d.ts` and `jsconfig.json`;
2. installs them into the laptop's `~/.config/herdr-app` (`$HERDR_APP_HOME`), keeping a different existing `layouts/trellis.json` as `trellis.json.bak`;
3. runs `herdr-app check` on both mods, when `herdr-app` is on PATH;
4. switches the app to the `trellis` layout and opens the side panel on Files.

It writes nothing else: no shipped mod or layout, and no token (the mod reads the mini's daemon through `trellis api` there). It stops with a message when ssh fails or the mini lacks a piece.

**Update:** run `node build.mjs --install` on the mini, then run the same line on the laptop. The laptop needs no Node, npm or repository.

## Install on the machine that builds

```sh
cd herdr-mod
npm ci                       # esbuild 0.28.2 (pinned, with the lockfile)
node build.mjs --install     # bundles dist/main.js and installs mods/trellis (with setup-viewer.sh) and layouts/trellis.json
herdr-app check ~/.config/herdr-app/mods/trellis
```

### The Files panel that publishes its selection

Herdr's shipped mods are read-only, so clone `files` and apply the patch:

```sh
herdr-app do mod.clone '{"name":"files","as":"trellis-files"}'
patch -d ~/.config/herdr-app/mods/trellis-files -p1 < trellis-files/publish-selection.patch
herdr-app check ~/.config/herdr-app/mods/trellis-files
```

The patch adds one shared value. When the panel shows a file, it sets `trellis.selectedFile` to `{root, path, on}`: the panel's folder, the file's path inside it, and its machine (`null` for this computer). When no file is shown, it sets the value to `null`. Everything else is the shipped mod.

To use the panel, open it as a pane:

```sh
herdr-app do pane.split '{"direction":"right","mod":"trellis-files"}'
```

Or use the `trellis` layout that `node build.mjs --install` installs from `layout/trellis.json`: a clone of Herdr's `main` whose side panel holds `trellis-files` (in the Files slot) or Changes, with the Trellis pane beside it. A different existing `layouts/trellis.json` is kept as `trellis.json.bak`. Switch to it with `herdr-app do layout.switch '{"name":"trellis"}'`.

The tab bar shows a Files button only when shared `"workspace.panels"` includes `"files"`, and the welcome can leave just `["changes"]`. So when the Trellis pane starts in a layout whose Files slot is `trellis-files`, it adds `"files"` there; see [DESIGN-NOTES.md](DESIGN-NOTES.md#reaching-trellis-files).

## Open the Trellis pane

```sh
herdr-app do pane.split '{"direction":"right","mod":"trellis"}'
```

Optional `props`:

| Prop | Default | Meaning |
|---|---|---|
| `url` | `http://127.0.0.1:8877` | The daemon. Only loopback (`127.0.0.1`, `localhost`, `[::1]`) is accepted. |
| `tokenFile` | `~/.config/trellis/serve.token` | The serve token file. |
| `checkout` | none | A root key to open while nothing is focused or selected yet. |
| `machine` | the shown space's machine | Herdr machine whose daemon to show; `""` means this computer. |
| `transport` | `fetch` here, `exec` elsewhere | `"exec"` uses `trellis api` on this computer too (no token; a 25 s timeout). |

For example, `{"direction":"right","mod":"trellis","props":{"url":"http://127.0.0.1:9000"}}`.

**Which daemon the pane shows:**

- For a space on this computer, the pane fetches from `url` here.
- For a space on another Herdr machine, the pane runs `trellis api` *on that machine* (`exec("trellis", ["api", …], { on: machine })`). The CLI reaches that machine's daemon over its private socket, as every Trellis command does, so no token is read by the mod or by a shell. `url` and `tokenFile` don't apply there.

**Which checkout the pane shows (#809).** The pane follows the focused agent. There is no checkout dropdown; the header names the checkout, and a **Following / Pinned** toggle stops and restarts the follow.

- **Focus an agent or terminal:** the pane switches to the deepest checkout the daemon lists that holds its folder (`focused().cwd`).
- **Focus a mod pane,** this one or Files included: the pane keeps the last agent-or-terminal checkout.
- **Select a file in `trellis-files`:** the pane switches to that file's checkout. Whichever happened last, the focus or the selection, wins.
- **Pinned:** agent focus is ignored. A Files selection made after pinning still switches the pane, because it is an explicit choice.
- **A folder in no listed checkout,** such as a fresh clone the daemon hasn't seen: the pane shows a note naming the folder and opens nothing. It never guesses a nearby checkout.
- **At start,** with nothing focused or selected, it opens the checkout shown last time (or `checkout` from props), if the daemon still lists it.

Reading a checkout's status starts indexing it, so only checkouts an agent, the Files panel or that last choice point at are ever read.

## Update

- **The laptop:** see [Install on a laptop](#install-on-a-laptop-that-views-this-mac-mini).
- **This mod:** pull the repository, then run `node build.mjs --install` again. Saving `main.js` reloads every open copy.
- **`trellis-files`:** Herdr never updates clones. After a Herdr update changes the shipped Files mod, remove `~/.config/herdr-app/mods/trellis-files`, then clone and patch again. If the patch no longer applies, re-make the same three-line change in the new `show()`.

## Check

- After every change, run `herdr-app check ~/.config/herdr-app/mods/trellis` and `herdr-app status`. Then look at the pane with `herdr-app shot <file>`.
- `node test/harness.mjs --url <daemon> --token-file <file> [--cwd <folder>] [--select <path>] [--click layer-<name>]` runs the real `src/main.js` in Node. It runs against a live daemon, with stand-ins for Herdr's modules, and prints the text the pane would show and the drawing it wrote. It checks the data path, not the picture.

- `node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/mod.test.mjs herdr-mod/test/view.test.mjs herdr-mod/test/parity.test.mjs herdr-mod/test/setup.test.mjs`, run from the repository root, tests the logic. `view.test.mjs` runs the real pane against a fake daemon through the `trellis api` transport, with a stand-in `trellis` that prints and exits as the CLI does. `setup.test.mjs` runs `setup-viewer.sh` against a stand-in mini (an `ssh` script over a temporary home). They need no install, and `tools/verify` runs them in the `checks` lane.

The build itself isn't part of `tools/verify`, because it needs `npm ci`, which downloads esbuild.
