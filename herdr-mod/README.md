# Trellis for Herdr

A [Herdr app](https://herdr.dev) mod that shows Trellis in a pane beside the coding agents (#707). It draws the loopback daemon's **Architecture** layer stack the same way the web app does, lists the layers with their counts, and follows the file selected in the `trellis-files` panel.

- `src/main.js` is the pane: a checkout selector, index status, typed error states, the diagram and the layer list.
- `src/client.js` is the daemon client: token, routes and typed errors.
- `src/diagram.js` draws the diagram. It uses the browser's own `web/architecture-pele.js`, `web/metric-heat.js`, `web/architecture.js` helpers and the vendored Pele, bundled unchanged.
- `src/dom.js` is the small SVG DOM the adapter needs, because the mod runtime has none.
- `trellis-files/publish-selection.patch` is the one change to a clone of Herdr's shipped Files mod.

See [DESIGN-NOTES.md](DESIGN-NOTES.md) for how it works, what's missing and what comes next.

## Requirements

- The Herdr app, with mods in `~/.config/herdr-app/mods/` (or `$HERDR_APP_HOME/mods/`).
- Node 24 and npm, to build.
- A Trellis daemon listening on loopback. The mod talks to `http://127.0.0.1:8877` with the token in `~/.config/trellis/serve.token`, the file `trellis serve` was started with:

  ```sh
  trellis serve --token-file ~/.config/trellis/serve.token
  ```

## Install

```sh
cd herdr-mod
npm ci                       # esbuild 0.28.2 (pinned, with the lockfile)
node build.mjs --install     # bundles dist/main.js and copies it to ~/.config/herdr-app/mods/trellis
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

Or point a layout's Files block at it. For a shipped layout, clone the layout first (`layout.clone`), then set the Files block's `content.mod` to `trellis-files`.

## Open the Trellis pane

```sh
herdr-app do pane.split '{"direction":"right","mod":"trellis"}'
```

Optional `props`:

| Prop | Default | Meaning |
|---|---|---|
| `url` | `http://127.0.0.1:8877` | The daemon. Only loopback (`127.0.0.1`, `localhost`, `[::1]`) is accepted. |
| `tokenFile` | `~/.config/trellis/serve.token` | The serve token file. |
| `checkout` | none | A root key to open. |

For example, `{"direction":"right","mod":"trellis","props":{"url":"http://127.0.0.1:9000"}}`.

Opening the pane doesn't choose a checkout by itself, because reading a checkout's status starts indexing it. It opens:

- the checkout chosen last time;
- otherwise the checkout that holds the focused terminal's folder;
- otherwise, nothing, until you choose a checkout in the header or select a file in `trellis-files`.

## Update

- **This mod:** pull the repository, then run `node build.mjs --install` again. Saving `main.js` reloads every open copy.
- **`trellis-files`:** Herdr never updates clones. After a Herdr update changes the shipped Files mod, remove `~/.config/herdr-app/mods/trellis-files`, then clone and patch again. If the patch no longer applies, re-make the same three-line change in the new `show()`.

## Check

- After every change, run `herdr-app check ~/.config/herdr-app/mods/trellis` and `herdr-app status`. Then look at the pane with `herdr-app shot <file>`.
- `node test/harness.mjs --url <daemon> --token-file <file> [--cwd <folder>] [--select <path>] [--click layer-<name>]` runs the real `src/main.js` in Node. It runs against a live daemon, with stand-ins for Herdr's modules, and prints the text the pane would show and the drawing it wrote. It checks the data path, not the picture.

- `node --import ./herdr-mod/test/register.mjs --test herdr-mod/test/mod.test.mjs`, run from the repository root, tests the logic. It needs no install, and `tools/verify` runs it in the `checks` lane.

The build itself isn't part of `tools/verify`, because it needs `npm ci`, which downloads esbuild.
