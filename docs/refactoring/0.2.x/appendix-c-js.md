# Refactoring analysis: `web/` (trellis-web) and `herdr-mod/` (trellis-herdr-mod)

> Analysis notes behind the [0.2.x refactoring spec](../0.2.x-spec.md), track W. Written by a read-only analysis agent at v0.2.0 (`d84f264a`); "I" is that agent. `decls.json` is the per-declaration metrics dump taken for the spec's baseline; it is not in the repository.

Read-only analysis of `main` at v0.2.0 (d84f264a). Metrics come from `decls.json` (metrics:v1, production declarations). Where a claim was not checked against source it says "not verified".

## 0. Headline findings

1. **`web/` has no module system.** It is 15 classic `<script defer>` files plus vendored Pele (`web/index.html:13-29`), no bundler (`web/package.json` is only `{"name":"trellis-web","private":true}`). Modules talk through `window.Trellis*` globals and through callbacks injected by `app.js`. Trellis's own references tool therefore sees **zero** intra-`web/` edges, so intra-app layers in `rules.toml` would stay vacuous for `web/` today.
2. **The Herdr mod already reuses the web projection code.** `herdr-mod/web-scripts.mjs:16-29` bundles eight `web/` scripts unchanged (Pele, metric-heat, architecture, architecture-apps, architecture-deviations, architecture-pele, sequence, sequence-pele) plus colour tokens from `style.css` and control options from `index.html`. The mod does not re-implement diagram projection, validation or the deviations model. What it re-implements is necessarily different (a DOM shim, hit-testing, gpui widgets), apart from about 60 lines of DTO helpers in `herdr-mod/src/client.js`.
3. **The cost of that reuse is hidden in file shape.** Three web files mix a pure part with a DOM controller, and the mod has to load the whole file with a fake `document` to get the pure part: `architecture.js` (1,645 lines, for `createDisplay`), `architecture-deviations.js` (746 lines, for `model`), `sequence.js` (797 lines; the mod's own comment says "its own SVG renderer is unused"). Splitting pure from DOM is the single structural change that helps both apps.
4. **There is one real dependency cycle**: `IndexPin` is defined in `app.js:12-29` (the top of the stack) and read by five lower modules (36 uses). Five test files regex-extract it out of `app.js` source to run a view in isolation.
5. **The workbench redesign (#303) is mostly already landed**: #304-#311 are closed. The current code is the redesign's output, so little of it is about to be thrown away. Still open: #312 (remote daemons over SSH), #313 (Herdr-client probe), #512 (class relationships).
6. **A possibly dead mode in the worst file.** `architecture-pele.js` `project`/`validatedSvg` carry a non-`display` mode (Pele-drawn edges, outer box, peer wrappers). Both production call sites (`web/architecture.js:837`, `:929`) go through `stack()` (`architecture.js:310`), which always passes `display`. The non-display path looks test-only (19 `project(` calls in `tests/architecture-pele-ui.test.cjs`). The mod's call (`herdr-mod/src/diagram.js:87`) was not traced to its options: verify before dropping.

---

## 1. Module dependency structure

### 1.1 `web/`

**Mechanism.** Each file except `sequence.js` and `app.js` is an IIFE that publishes one global (`classes.js` has two IIFEs and publishes two). `app.js` and `sequence.js` are un-wrapped classic scripts: their top-level `function`/`let`/`const` live in the page's global scope.

- Globals published: `TrellisSequence` (`sequence.js:797`), `TrellisPeleSequence` (`sequence-pele.js:408`), `TrellisShell` (`shell.js:459`), `TrellisPeleClasses` (`classes.js:141`), `TrellisClasses` (`classes.js:709`), `TrellisNavigation` (`navigation.js:378`), `TrellisMetricHeat` (`metric-heat.js:58`), `TrellisMetrics` (`viewer-metrics.js:291`), `TrellisFiles` (`files.js:170`), `TrellisArchitecturePele` (`architecture-pele.js:820`), `TrellisArchitectureApps` (`architecture-apps.js:344`), `TrellisDeviations` (`architecture-deviations.js:745`), `TrellisArchitecture` (`architecture.js:1644`), `TrellisTreemapPele` (`treemap-pele.js:221`), `TrellisTreemap` (`treemap.js:317`), `TrellisIndexPin` (`app.js:29`), and `window.pele` from the vendor file.
- **State sharing.** `app.js` holds the page state as top-level `let`s: 41 `let` statements declaring roughly 103 mutable variables (`token`, `epoch`, `selectedRootKey`, `status`, `section`, the `*Serial` tickets, ...). View controllers get what they need by dependency injection: `TrellisMetrics.create({...})` (`app.js:2466`), `TrellisTreemap.create` (`:2473`), `TrellisArchitecture.create` (`:2481`), `TrellisNavigation.init` (`:3152`), `TrellisClasses.init` (`:3164`), each passed `request`, `context()` (rootKey/session/pin), `readSource`, `onConflict` callbacks. There is no shared context object; `context()` closures are the nearest thing.
- Tests rely on the global scope: seven node tests concatenate `["sequence.js","shell.js","files.js","app.js"]` into one `vm` context and poke `app.js` internals (`tests/browse-ui.test.cjs:8`, `files-ui:9`, `token-ui:8`, `dependency-ui:8`, `external-source-ui:8`, `app-switcher-ui:13`, `shell-ui:27`).

**Who uses whom** (count of `window.X` references, from grep; own-definition lines removed):

| File | Uses |
| --- | --- |
| `app.js` | Shell 34, Sequence 22, Classes 14, Files 8, Navigation 7, PeleSequence 4, ArchitectureApps 1, Architecture 1, Metrics 1, Treemap 1 |
| `architecture.js` | IndexPin 14, Shell 6, ArchitecturePele 4, ArchitectureApps 2, Deviations 1, MetricHeat 1 |
| `architecture-deviations.js` | Classes 2 (`showContextMenu`, `:666-668`), IndexPin 1 |
| `classes.js` | IndexPin 6, PeleClasses 3 (same file) |
| `navigation.js` | IndexPin 9 |
| `sequence-pele.js` | Sequence 16 |
| `shell.js` | Sequence 9, Classes 2, Navigation 1 |
| `treemap.js` | IndexPin 6, TreemapPele 4, Shell 2, MetricHeat 1 |
| `viewer-metrics.js` | Shell 2, MetricHeat 1 |
| `architecture-pele.js`, `treemap-pele.js`, `classes.js` (adapter half), `sequence-pele.js` | `window.pele` |
| `architecture-apps.js`, `metric-heat.js`, `files.js`, `sequence.js` | nothing (leaves) |

**Implicit layering.** It exists and is mostly followed:

| Layer | Files |
| --- | --- |
| L0 vendor | `vendor/pele-1.10.0-trellis.2.js` |
| L1 pure, DOM-free | `metric-heat.js`, `architecture-apps.js`; plus the pure halves of `architecture.js` (`createDisplay` and helpers, lines 12-378), `architecture-deviations.js` (`model`, lines 13-247), `sequence.js` (lines 1-440) |
| L2 Pele adapters (project, validate, decorate) | `architecture-pele.js`, `sequence-pele.js`, `treemap-pele.js`, first IIFE of `classes.js` (lines 4-142) |
| L3 view controllers | `architecture.js` `create` (380-1643), `architecture-deviations.js` `create` (249-743), `treemap.js`, `viewer-metrics.js`, second IIFE of `classes.js` (143-710), `navigation.js`, `files.js`, `sequence.js` `drawSequence` (457-795) |
| L4 shell chrome and inspector | `shell.js` |
| L5 composition root, page state, routing | `app.js` |

**Where it breaks:**

- **Cycle: `app.js` <-> views via `IndexPin`.** L3 files read `window.TrellisIndexPin`, which L5 defines. 36 uses in `architecture.js`, `navigation.js`, `classes.js`, `treemap.js`, `architecture-deviations.js`. Tests work around it by regex: `tests/classes-ui.test.cjs:9`, `architecture-ui.test.cjs:8`, `architecture-apps-ui.test.cjs:8`, `navigation-ui.test.cjs:9`, `treemap-ui.test.cjs:8`.
- **Controllers call up into the shell**: `architecture.js` (6: `showArchitectureInspector` etc.), `treemap.js` (2), `viewer-metrics.js` (2). The shell in turn calls down into `TrellisSequence` (9), `TrellisClasses.closeContextMenu` and `TrellisNavigation.reset`. So shell and views are mutually dependent, softened by `?.`.
- **A shared widget lives in a view**: the context menu is `TrellisClasses.showContextMenu` (`classes.js:228`) and is used by the deviations view (`architecture-deviations.js:666`) and by navigation through `app.js:3161`.
- **Four files span two layers** (listed in L1/L3 above), which is why the mod pulls controllers in.
- **Global-scope collision**: `sourceWitness` is declared at top level in both `sequence.js:7` and `app.js:1194`. The bodies are byte-identical, so the later one silently replaces the earlier one for every caller in `sequence.js`. It is one edit away from a behaviour change in the other file. Further private copies: `navigation.js:76`, `shell.js:234`, `sequence-pele.js:14` (`witness`), and a shorter variant `classes.js:21`.
- **Files everything touches**: `app.js` (hub, by design), `shell.js` (4 dependants), `IndexPin` (5 dependants), `sequence.js` helpers (3 dependants).

### 1.2 `herdr-mod/src`

Real ES modules, bundled by esbuild (`build.mjs:27-43`). The graph is a clean DAG with no cycles:

- `main.js` -> `client.js`, `diagram.js`, `architecture-ui.js`, `sequence.js`, `panels.js`, host modules (`gpui-kit`, `gpui-base`, `herdr`)
- `architecture-ui.js` -> `diagram.js`, host
- `diagram.js` -> `dom.js`, `runtime.js`, `trellis-web:{metric-heat, architecture-apps, architecture, architecture-pele, architecture-deviations, tokens, controls}`
- `sequence.js` -> `dom.js`, `runtime.js`, `trellis-web:{sequence, sequence-pele}`
- `runtime.js` -> `dom.js`, `trellis-web:pele`
- `client.js`, `panels.js`, `dom.js` -> host or nothing

Layering: host UI (`main.js`, `architecture-ui.js`, `panels.js`) -> adapters (`diagram.js`, `sequence.js`) -> platform (`runtime.js`, `dom.js`), with `client.js` as a side leaf. The only structural smell is `main.js`: one `Trellis extends View` class of 988 lines and about 50 methods (file cognitive sum 664 of the app's 1,324), holding checkout following, status, architecture, deviations, sequence and all rendering.

---

## 2. Intra-app layers in `rules.toml`?

### `web/`: not yet. The structure is there; the evidence is not.

The files mostly follow the layering in 1.1, but the rules check works on reference edges, and `trellis_references` with `prefix:"web"` returned 16 modules and **0 groups, 0 mapped sites**. Dependencies through `window.TrellisX.fn()` are not import edges. Authoring layers now would report 0 deviations for the wrong reason. Whether fresh scip-typescript call bindings could carry these edges was not verifiable: the JS semantic evidence reported `possiblyStale` and `trellis_outgoing_calls` failed with `store_unavailable`.

Layers become checkable only after one of: (a) `web/` moves to ES modules, or (b) the rules engine counts semantic call bindings through globals. Option (a) is large: 17 node test harnesses load sources with `vm.runInNewContext`, and the mod's wrapper (`web-scripts.mjs:57`) assumes classic scripts.

If the layers in 1.1 were enforced today, these are the deviations that would appear (from the grep matrix, so they are known work whatever the tooling does):

| Deviation | Sites | Fix |
| --- | --- | --- |
| L3 -> L5: views read `TrellisIndexPin` from `app.js` | 36 | extract `web/index-pin.js` (L1) |
| L3 -> L4: `architecture.js`, `treemap.js`, `viewer-metrics.js` call `TrellisShell.show*Inspector` | 10 | inject an `inspector` callback at `create()`, as `onInspect` already is |
| L4 -> L3: `shell.js` calls `TrellisClasses.closeContextMenu`, `TrellisNavigation.reset` | 3 | callbacks registered by `app.js`, or move the menu to its own file |
| L3 -> L3 peer: `architecture-deviations.js` -> `TrellisClasses.showContextMenu` | 2 | extract `web/context-menu.js` |
| Mixed-layer files: `architecture.js`, `architecture-deviations.js`, `sequence.js`, `classes.js` | 4 files | split (see 6, item 3) |

### `herdr-mod/`: yes, cheap, but small value.

ES imports exist, so the check would be real. Proposed:

- `herdr-ui` (order 1): `herdr-mod/src/main.js`, `architecture-ui.js`, `panels.js`
- `herdr-adapters` (order 2): `herdr-mod/src/diagram.js`, `sequence.js`, `client.js`
- `herdr-platform` (order 3): `herdr-mod/src/runtime.js`, `dom.js`
- hidden tooling: `herdr-mod/build.mjs`, `web-plugin.mjs`, `web-scripts.mjs`, `herdr-mod/test/**`

Deviations today: 0 (checked against every `import` line). With eight source files this guards little. The more useful rule is cross-app: "the mod imports only web's pure and adapter layers". Today it would flag `trellis-web:architecture` and `trellis-web:architecture-deviations` (controller files) and `trellis-web:sequence` (carries the fallback renderer), which are exactly the split candidates. Whether Trellis resolves the `trellis-web:` virtual specifier to `web/` files was not verified (`trellis_references` on `herdr-mod` failed with `store_unavailable`); it probably does not, since the mapping lives in an esbuild plugin.

---

## 3. Duplication

### 3a. Inside `web/`: the Pele adapters

Four adapters share one pipeline: `project` (DTO -> Mermaid source + identity maps), `pele.render`, `validatedSvg` (admit the SVG and bind nodes back to DTO records), `render` (decorate and install).

**`validatedSvg` x4.** Each starts with the same admission prefix, then a view-specific identity check.

| Step | architecture-pele | sequence-pele | treemap-pele | classes |
| --- | --- | --- | --- | --- |
| 8 MiB byte cap | 289 | 211 | 105 | 75 |
| root prefix + theme signature | 290-291 | 212-218 | 106-109 | 76-78 |
| strip signature, rewrite `style="rx:var(--_r)"` | 292 | 219-220 | 110 | 79-80 |
| reject inline style | 293 | 221 | 111 | 81 |
| reject active markup before parsing | 296 | **absent** | 111 | 83 |
| DOMParser + parsererror | 297-298 | 222-223 | 112-113 | 84-85 |
| 12,000-node, tag and attribute allowlist loop | 299-303 | 224-230 | 116-120 | 86-90 |

About 15-20 lines per file are the same code with different error words. The remaining 20-50 lines are genuinely view-specific (nodes and edges; notes, messages, lanes and frames; tile geometry; classes and members) and are the bulk of each function's complexity. Rough estimate: the shared prefix is 25-30 of each function's 60-125 cognitive points.

Two real inconsistencies the duplication has produced:
- `sequence-pele.js:221` has no pre-parse rejection of `<script|foreignObject|image|a>`; the other three do. The post-parse tag allowlist still rejects those elements, so this is a consistency gap rather than a hole. The comment at `architecture-pele.js:294` explains why the pre-parse check exists (CSP reports from detached parsing).
- The pre-parse regexes differ: architecture rejects `script|style|foreignObject|image|a`, classes omits `style`, treemap adds `<!`.

**Other copies in the same four files:**
- `ROOT_STYLE`/`THEME` theme string: `architecture-pele.js:5`, `sequence-pele.js:4`, `classes.js:7`, `treemap-pele.js:5`
- `tags`/`attrs` allowlist sets: 3 near-identical, plus treemap's own (`treemap-pele.js:114-115`)
- `unsupported()` x4; `encode` x2 (`sequence-pele.js:13`, `classes.js:16`); `bytes` x2
- canvas text measurer x3 (`architecture-pele.js:675-678`, `treemap-pele.js:172`, `classes.js:122-128`)
- "make it a button" (role, tabindex, aria-label, click, Enter/Space keydown): 9 occurrences across `architecture-pele.js` (2), `sequence-pele.js` (2), `sequence.js` (3), `treemap-pele.js` (1), `classes.js` (1)
- palette literals (`#14151D`, `#0B0C10`, `#F7F7FB`, `#777A8E`, `#B8BAC8`) repeated in `architecture-pele.js` and `treemap-pele.js`
- status vocabulary differs: architecture and treemap return `ready|error|stale` and paint their own error; sequence and classes return `rendered|unsupported|stale` and let the caller fall back.

**`project` x4**: nothing shared beyond the pattern (validate DTO, emit Mermaid, cap bytes). View-specific; leave separate.

**`render` x4**: the skeleton (currency guard, fonts ready, measurer, Pele availability, project -> render -> validate -> decorate -> install, catch) is the same shape, but each differs in async points and fallbacks. Share the helpers, not a framework.

**Verdict:** about 15-20% of adapter code is truly shared. Extract one small file, `web/pele-svg.js`, with `unsupported`, `encode`, `bytes`, `THEME`, `admit(raw, {rootClass, themes, tags, attrs, activeMarkup, label})`, `asButton(node, label, onActivate)` and `textMeasurer(font)`. Do not build a generic adapter base.

**Outside the adapters:** `sourceWitness` x5 (see 1.1); the DOM helper `make`/`el`/`element` x9 (`architecture.js:5`, `architecture-deviations.js:7`, `treemap.js:5`, `viewer-metrics.js:5`, `files.js:4`, `shell.js:7`, `navigation.js:10`, `app.js:618`, plus the SVG variant `sequence.js:462`); `finite`/`display` in several controllers.

### 3b. Between `web/` and `herdr-mod/src`

| Mod code | Relation to web | Verdict |
| --- | --- | --- |
| `diagram.js` `draw` (105), `sequence.js` `drawSequence` (27) | call web's `architecture-pele.render` / `sequence-pele.render` over the DOM shim | reuse, no duplication |
| `diagram.js` `summarize` (42) | wraps web's `createDisplay` and `architecture-apps` | reuse; comments at `:45` and `:136` say it mirrors `scopeGroup` and `drawApps` in `architecture.js` (extent not verified) |
| `deviationModel` (`diagram.js:26`) | web's `TrellisDeviations.model` | reuse |
| `dom.js` (250 lines: `Element`, selector subset, `parseXml`, `serialize`) | no DOM in Herdr's runtime | necessarily different |
| `diagram.js` `boxes` (203), `segments` (246), `hit` (275); `sequence.js` `boxes` (62) | the mod draws an image and hit-tests rectangles | necessarily different |
| `architecture-ui.js` (controls, switcher, legend, inspector, deviations tree) | gpui widgets instead of DOM; consumes the shared models | necessarily different view code |
| `main.js` controller | parallels `app.js` and `architecture.js` `create`, in a different host model (`cx.notify`) | necessarily different |
| `client.js` `pinOf`/`samePin`/`pinLabel` (150-157) | copy of `IndexPin` (`app.js:12-28`) | **duplicate** |
| `client.js` `jobCause` (202) | identical to `app.js:998` | **duplicate** |
| `client.js` `indexLabel` (178) | copy of `checkoutIndexLabel` (`app.js:953`), **already drifted**: web says `"Failed"`, the mod says `"Unavailable · <indexError>"` | **duplicate, drifted** |
| `client.js` `architecturePayload` (160), `detailPage` (191) | re-statements of `architecture.js` `payloadOf` (483) and `details` (524) | **duplicate** (bodies not compared line by line) |

**Could a shared pure-projection module serve both?** It already does in effect. Make it explicit: keep pure code in files that need no `document`, and let the mod import only those. Candidates: `metric-heat.js`, `architecture-apps.js` (already pure), new `architecture-display.js` (from `architecture.js:12-378`), new `deviations-model.js` (from `architecture-deviations.js:13-247`), new `sequence-model.js` (from `sequence.js:1-440`), new `index-pin.js`, and a small `architecture-dto.js` for `payloadOf`/`details` validation. That removes the `client.js` copies and three of the mod's fake-`document` parameters.

**What the pipeline allows.**
- Daemon: every web file is a hand-written route with `include_str!`, in **two** routers: the `test-support` `router()` (`src/http.rs:2289`) and `DaemonState::router` (`src/http.rs:3706`). A new web file costs one `<script>` tag, two 9-line route blocks, one `node --check` entry (`tests/factory-config.test.cjs:305`) and edits to the test harness file lists. A table-driven asset list in `http.rs` would make every split below cheaper (Rust side, out of my scope).
- Mod: `web-scripts.mjs` wraps a classic script as `export default function(window, document, ...) { <source>; return window.X }`. A new shared file needs one entry in the `WEB` table and the consumer must put its result on the `window`/`env` object passed to dependants (`diagram.js:23` already does this for `TrellisMetricHeat` and `TrellisArchitectureApps`).
- ES modules without a bundler would work in the browser and simplify the mod, but the node tests and the wrapper are built around classic scripts. Not recommended now.

---

## 4. Hot functions

Format: why complex; decomposition; risk; pinning tests; size; expected max cognitive after.

### `web/architecture-pele.js`

Tests for the whole file: `tests/architecture-pele-ui.test.cjs`, `tests/architecture-ui.test.cjs` (2,333 lines), `tests/architecture-apps-ui.test.cjs`, `tests/browser/architecture-pele.test.cjs`, `architecture-routing.test.cjs`, `architecture.test.cjs`, `architecture-apps.test.cjs`, `pele-block-labels.test.cjs`; and through the bundle `herdr-mod/test/parity.test.mjs`, `view.test.mjs`, `mod.test.mjs`.

**`render` :665 (252/102/17/155).** Six jobs in one `try`: measurer setup, the width-fit search (686-728: project, render, bisect chip width, drop a column, widen the stack span), validation, palette substitution, a seven-way `if/else if` on `item.kind` that styles each node and picks its callback (750-786), and wiring of button semantics (788-803). `if (pause && !(await breathe())) return {status:"stale"}` appears eight times.
- Helpers: `fitLayout(view, options, measure, checkpoint)` -> `{projection, raw}`, with a pure `nextFit(state, fits, drawnWidth, target, cells)` step function; `applyPalette(root)`; `noteIdentities(projection)`; a `decorate` table keyed by kind (`C`, `M`, `D`, `U`, `N`, `L`, `E`) returning `{callback, expanded}`; `bindControl(node, item, callback, expanded)`; `bandFrames(controls)`; a `checkpoint()` that throws a stale sentinel caught once.
- Risk: medium. The fit loop has a stateful subtlety ("Projecting charges the caller's chip budget", :722) that the re-projection preserves. Geometry is pinned by browser tests.
- Size M. Expected max about 25 (`fitLayout`).

**`route` :494 (196/63/12/156).** Five phases in one function: classify each edge into a plan by row relation (508-553, a five-way nested chain), place runs and size gutters (555-571), compute channel offsets with a left/right mirror (573-591), shift every band (593-609), then build a polyline per plan kind (624-648, a second five-way chain).
- Helpers: `planEdge` dispatching to `planAdjacent`, `planPeer`, `planLongDown`, `planLongUp`, `planShared`, `planBoxes`; `sizeGutters(gutters, rows)`; `channelOffsets(layout, stack, sharedFrames, left, right)`; `shiftBands(controls, layout, offsets)`; a `pathFor` table keyed by plan kind. `channelReserve` (:477) re-counts lanes "exactly as routing does": share one `countLanes(projection)` so the two cannot drift.
- Phases 1-3 and 5 are arithmetic on frames. Moved to a DOM-free `web/architecture-routing.js` they become unit-testable in node; today only `tests/browser/architecture-routing.test.cjs` reaches them.
- Risk: medium-high (visual output; 16 commits since August, last substantive change #949/#950 on 2026-11-12). Size M-L. Expected max about 30 (`planEdge` family).

**`project` :90 (166/89/5/198).** One closure with 12 nested functions sharing `source`, `nodes`, `host`, `serial`; DTO validation interleaved with emission (layers 127-130, modules 154-157, edges 260); and two modes braided together by about 25 `display ?` branches.
- First decide the non-display mode (finding 6). If it goes, `project` loses the outer box, the peer wrapper, the unassigned fallback (253-257) and the Pele edge source line (278), and `validatedSvg` loses 316-335.
- Then: `validateView(view)`; `labelFactory(measure, chip)` (98-111); `bandGeometry` (135-140); a small emitter `{open, leaf, close, source}`; `emitStack`, `emitSharedBoxes`, `emitUnassigned` (242-252); `projectEdges` (258-279); `layoutOf` (284-285).
- Risk: medium (placeholder ids are positional through `serial`, so emission order must not change). Size M. Expected max about 30, about 20 if the mode is dropped.

**`validatedSvg` :288 (90).** Shared admission prefix plus `bindNodes` (304-315), `bindPeleEdges` (316-335, possibly dead), count checks. Size S after `pele-svg.js` exists. Expected about 20.

### `web/sequence-pele.js`

Tests: `tests/sequence-ui.test.cjs` (1,125 lines), `tests/browser/sequence-pele.test.cjs`, `sequence.test.cjs`, `sequence-callbacks.test.cjs`, `sequence-readable.test.cjs`, `sequence-cost.test.cjs`, `sequence-interactions.test.cjs`, `sequence-hops.test.cjs`; `herdr-mod/test/mod.test.mjs`.

**`visit` closure :111 (165).** A recursive walk that validates each step (114-117), validates collapsed members (126-129), chooses a frame from six overlapping conditions (130-145), emits open/row/activation/children/alternate/return/close, and enforces caps.
- Helpers: `checkStep(step)`, `checkCollapsed(members)`, `previewOf(step, expanded)`, `frameFor(step, displayProjection, options)` -> `{frame, heading, header, kind}` as a pure function, `emitBody(step, ...)`.
- Risk: medium. Pele statement ids are derived from emit order (198-206), so every `emit` must stay in sequence; the tests cover this heavily. Size M. Expected about 25.

**`add` closure :54 (60).** Label assembly in three variants. Helpers: `stepLabel(step)`, `previewLabel(step, preview, label)`, `rowDisplay(...)`. Size S. Expected about 15.

**`render` :275 (90).** Helpers: `bindLaneActors` (297-311), `bindRow` (314-336), `attachGroupToggle` (337-380), `bindReturns` (383-388), `reserveControlSpace` (390-398). Low risk. Size S-M. Expected about 15.

**`validatedSvg` :210 (83).** Prefix to `admit`; then `checkNotes`, `checkMessages`, `checkLanes`, `checkFrames`. Size S. Expected about 15.

### `web/treemap-pele.js`

Tests: `tests/treemap-pele.test.cjs`, `tests/treemap-ui.test.cjs`, `tests/browser/treemap.test.cjs`. Churn 3: lowest of the adapters.

- **`validatedSvg` :104 (125).** Much of the score is long `||` chains in flat validations. Helpers: `admit`, `checkViewport` (121-122), `checkTile(node, item, kind, bounds)` (131-140), `checkLabel` (141-145), `checkTotals` (149). Size S-M. Expected about 25.
- **`project` :16 (72).** Helpers: `validFile(file)` (the same test is written twice, :20 and :44), `ensureDirectory` (22-42), `addFile`, `emitTree` (63-84). Size S. Expected about 20.
- **`render` :166 (66).** Helper `decorateTile(control, ctx, options)`. Size S. Expected about 15.

### `web/classes.js`

Tests: `tests/classes-ui.test.cjs` (690 lines), `tests/browser/classes-pele.test.cjs`, `classes-app.test.cjs`, `classes-languages.test.cjs`.

- **`lookup` :352 (92).** A paged read loop that skips other apps' classes (376-390), a result-list reset, result rendering, a five-branch nested ternary for the status sentence (:413), and ticket/choice guards throughout. Helpers: `readClassPages(params, choice, guard)`, `clearResults()`, `renderResults(items, next, c)`, `lookupMessage({unindexed, items, choice, data, appNote})` (pure). Risk medium (three tickets: `searchSerial`, `serial`, `lastLookup`). Size M. Expected about 25.
- **`project` :26 (76)** and **`validatedSvg` :74 (60)**: `checkNode`, `memberLines(kind, list)`; `admit` + `checkClass(group, item)`. Size S each, expected about 20. #512 (class relationships, open) will rewrite both, since the validator rejects any edge (`:93`): defer.

### `web/sequence.js` (fallback SVG renderer)

Tests: `tests/sequence-ui.test.cjs`, `tests/browser/sequence*.test.cjs`.

**`drawSequence` :457 (56, 339 lines), `sourceControl` :580 (73), `draw` :670 (95).** A hand-drawn renderer used only when Pele returns `unsupported` (`app.js:2102`, `:1604`). `draw` contains four near-identical frame blocks (loop 678-689, branch/callee 691-716, group 718-744, fragment 762-775), each building the box, the clipped tab path, the label, then recursing.
- First question is whether the fallback stays a product requirement (owner decision; I found no ruling, not verified in the decision records). If it goes, about 340 lines and three hot functions go with it.
- If it stays: move it to `web/sequence-svg.js` (pure move, so the mod stops bundling it); helpers `collectLanes` (470-508), `drawHeaders` (533-553), `rowLabel(step, display, evidence, operator, capacity)` as a pure function (590-617), `frame(group, x, start, operator, heading)` shared by the four blocks, `drawArrow`. Risk medium. Size L. Expected about 30.

### `web/navigation.js`

**`open` :87 (80).** Tests: `tests/navigation-ui.test.cjs` (795 lines), `tests/browser/navigation-resolved.test.cjs`. Menu lifecycle guards mixed with pure target filtering and labelling. Helpers: `rememberScroll(anchor)` (89-92), `admittedTargets(data, body)` (pure, 137-147), `targetLabels(candidates)` (pure, 148-153), `actionsFor(target, label, owner, run)` (154-176), `trailingNotices(data)` (177-181). Low risk. Size S-M. Expected about 20.

### `web/architecture.js`

Tests: `tests/architecture-ui.test.cjs`, `tests/architecture-apps-ui.test.cjs`, `tests/app-switcher-ui.test.cjs`, `tests/browser/architecture*.test.cjs`, `app-switcher.test.cjs`.

- **`drawApps` :888 (79).** Builds each app column, renders its stack, re-plans widths, places interface cards with three placement modes, draws the overlay. Helpers: `appColumn(part)` (902-921), `unlayeredBox(group)` (923-926), `bandPlaces(parts, boxes, grid, plan)` (937-941), `interfaceCard(entry, routed, plan, places)` (946-969), `routeOverlay(routed, plan, height)` (971-983), `noInterfaceNote(parts)`. Risk medium (reads live layout). Changed 2026-11-12 (#949/#950): let it settle. Size M. Expected about 20.
- **`inspect` :1281 (55, nesting 12).** An eight-way `if/else if` on `kind`. The nesting figure is the chain length, not real depth. Replace with a table `inspectors = {module, edge, layer, interface, app, unresolved, external, unlayered}`; `inspectInterface` (:1246) and `inspectApp` (:1266) already exist in that shape. Low risk. Size S. Expected about 10.

### `web/app.js`

Tests: `tests/browse-ui.test.cjs` (2,526 lines), `files-ui`, `token-ui`, `shell-ui`, `dependency-ui`, `external-source-ui`, `app-switcher-ui`; `tests/browser/connect`, `viewer-shell`, `stall-recovery`, `files`, `navigation-resolved`.

- **`readNavigationLocation` :258 (68).** History-entry restore by kind (286-311) plus hash-link parsing and validation (312-338) plus the sequence fetch (339-355). Helpers: `openRootView(current)` (272-285); `restoreEntry(entry, current)` as a per-kind table; `parseSequenceLink(hash, context)` as a pure function returning `{id, requested, boundary}` or throwing the existing messages; `openSequenceLink(link, current)`. Risk medium (#951 and #965 landed here on 2026-11-12). Size M. Expected about 20.
- **`refreshStatus` :760 (63).** The page's concurrency centre: serial and epoch guards, the `index_not_ready` recovery path (772-787), status adoption with invalidation (791-816), a status sentence (820-831), the queued pair refresh in `finally` (847-856). Helpers: `showIndexNotReady(error)`, `adoptStatus(data)` -> `{nextPin, browseChanged}`, `semanticSummary(languages, stats)` (pure), `statusSentence(status)` (pure), `drainQueuedPairRefresh(followup)`. Risk medium-high: ordering of the guards is the correctness rule. Size M. Expected about 20.
- **`setSection` :1384 (62, nesting 8).** Two unrelated bodies behind one early `return`. Helpers: `focusSection(name, view)`, `interruptOutlineReads()`, `enterDiagrams(view)` (1399-1416), `enterFiles()` (1418-1442). Low-medium risk. Size S. Expected about 20.

### `web/architecture-deviations.js`

**`closureNames` :158 (67).** Pure; three passes and two parent-walk loops in 22 lines. Tests: `tests/architecture-ui.test.cjs`, `herdr-mod/test/parity.test.mjs` (`:177`, #911). Helpers: `knownOwners(rows)`, `outlineById(outline)`, `enclosingCallable(byId, id)`, `reachesRoot(byId, id)`, `ownerRecord(item, byId, known)`. Low risk. Size S. Expected about 12.

### `herdr-mod/src`

Tests: `herdr-mod/test/view.test.mjs` (714 lines), `parity.test.mjs` (438), `mod.test.mjs` (171). Which test exercises which function was not traced individually.

- **`dom.js:151` `parseXml` (60).** A single-pass tokenizer; the branches are the grammar. Possible split: `skipMarkup` (comment, PI, CDATA, doctype) and `readOpenTag`. Churn 2. **Leave alone** unless it changes for another reason. Expected about 25 if split.
- **`architecture-ui.js:191` `deviations` (47).** Three nested loops building file, function and line rows with a shared row cap. Helpers: `deviationHead`, `chipRow`, `fileRow`, `functionRow`, `lineRow`, and a `treeRows(tree, state, cap)` generator. Size S. Expected about 15.
- **`main.js:835` `body` (44).** A flat ladder of 14 early returns for connection and index states, then the Architecture body. Helpers: `gate(t)` returning a status node or null (836-848), `architectureBody(t, file, place)`. Size S. Expected about 20.
- **`diagram.js:246` `segments` (31, nesting 8).** An SVG path-data reader, 28 lines. Leave alone.
- `main.js:543` `press` shows nesting 12 for a 15-line `else if` chain: a metric artefact, not a problem.

---

## 5. `web/app.js` responsibility map and split

Approximate ranges, from the top-level declaration list:

| Lines | Responsibility | Approx. lines |
| --- | --- | --- |
| 1-33 | page state; `IndexPin` (12-29) | 33 |
| 34-110 | pinned-head observation (`observePinnedHead`, `observePinnedResponse`) | 75 |
| 111-409 | history and link routing (`rememberNavigation`, `navigateDiagram`, `restoreSourceEntry`, `readNavigationLocation`, Deviations route, listeners) | 300 |
| 410-616 | agent view requests, follow-pinned, stall recovery (`observeViewRequest`, `recover`, `showAgentView`, `applyAgentView`) | 205 |
| 617-734 | DOM helpers and API client (`element`, `operationGuard`, `selectedUrl`, `api` :653, `perform`, `stale`, `clearSource`) | 118 |
| 735-875 | Git and status (`refreshGit`, `refreshStatus` :760, `unexpectedPair`) | 140 |
| 876-1192 | token and connect, checkout list and selection, index status (`loadCheckouts` :1046, `selectCheckout` :1119) | 317 |
| 1193-1293 | source pane (`showSource`, `paintCachedSource`, `showCachedSource`, `refreshView`) | 100 |
| 1294-1353 | index job polling | 60 |
| 1354-1740 | Files section (`setSection` :1384, outline, symbol, matches, mini sequence, basis and diff) | 387 |
| 1741-1936 | catalog browse (`loadFiles`, `renderFiles` :1839, `toggleFile`, `selectMethod`) | 196 |
| 1937-2206 | Sequence view controller (`openArchitectureSequence`, `highlightSequenceBoundary`, `loadSequence` :2039, file filter, fit and columns) | 270 |
| 2207-2320 | reveal, method lists, tree root and refresh | 114 |
| 2321-2372 | revision-bound reads (`revisionRead`, `architectureRequest`) | 52 |
| 2373-2465 | app switcher and inventory (#852) | 93 |
| 2466-2522 | view wiring (`TrellisMetrics/Treemap/Architecture.create`, view callbacks) | 57 |
| 2523-2737 | directory tree (`loadDirectory`, `renderDirectoryTree` :2647) | 215 |
| 2738-2822 | external sources | 85 |
| 2823-3031 | library catalog and dependencies | 209 |
| 3032-3113 | class support, view availability, Sequence start list | 82 |
| 3114-3179 | Classes and Navigation wiring, init | 66 |

**Proposed files** (classic scripts, same global scope):

| New file | From | Kind of change |
| --- | --- | --- |
| `index-pin.js` | 12-30 | pure move; loads before every view; fixes the cycle |
| `app-state.js` | the `let`/`const` state lines | pure move; must load first among app files |
| `app-api.js` | 617-734, 2321-2372 | pure move |
| `app-navigation.js` | 111-409 | pure move |
| `app-agent.js` | 34-110, 410-616 | pure move |
| `app-checkout.js` | 735-1192, 1294-1353 | pure move; **wait for #312** |
| `app-source.js` | 1193-1293, 2738-2822 | pure move |
| `app-files.js` | 1354-1740 except `setSection` | pure move |
| `app-tree.js` | 1741-1936, 2207-2320, 2523-2737 | pure move |
| `app-sequence.js` | 1937-2206, 3087-3112 | pure move |
| `app-libraries.js` | 2823-3031 | pure move |
| `app.js` (remaining) | `setSection`, 2373-2522, 3032-3086, 3114-3179 | about 400 lines of wiring |

**Why these are pure moves, and the one catch.** Top-level `let`/`const` in classic scripts share one global lexical scope, so moved functions still see the state, and the tests' concatenated-source harness keeps working once its file list is updated. The catch is load order: statements that run at load time (listener registration at 408-409, 888-906, 1159-1192; `const viewerMetrics = ...create(...)` at 2466; `initClassView` at 3177) must come after the `let`s they read, or they throw a temporal-dead-zone `ReferenceError`. Hence `app-state.js` first and wiring last. Each new file also costs the per-file overhead in 3b (two `http.rs` routes, one script tag, one `node --check` entry, seven test file lists).

**What this does not fix.** The roughly 103 shared mutable globals remain; the split makes ownership visible and gets every file under 600 lines, but real encapsulation (grouping state into `filesState`, `navigationState`, ... objects owned by one file) is a second, non-mechanical step. `app.js` has the highest churn in the repo's JS (99 commits since 2026-08-01, against 41 for the next file), so do the split in one quiet window to avoid conflicts with in-flight branches.

---

## 6. Cost-benefit ranking

Churn since 2026-08-01 (`git log --oneline | wc -l`): `app.js` 99, `architecture.js` 41, `shell.js` 31, `sequence.js` 30, `sequence-pele.js` 26, `classes.js` 23, `navigation.js` 19, `architecture-pele.js` 16, `treemap.js` 11, `viewer-metrics.js` 10, `files.js` 8, `architecture-deviations.js` 8, `architecture-apps.js` 6, `treemap-pele.js` 3, `metric-heat.js` 2; `herdr-mod/src/main.js` 5, `diagram.js` 4, `architecture-ui.js` 4, `client.js` 3, `dom.js` 2.

| # | Refactoring | Size | Benefit | Risk |
| --- | --- | --- | --- | --- |
| 1 | Extract `web/index-pin.js` | S | removes the only cycle; deletes the regex extraction in 5 tests; lets the mod drop `pinOf`/`samePin`/`pinLabel` | very low (pure move) |
| 2 | Split pure from DOM: `architecture-display.js`, `deviations-model.js`, `sequence-model.js` (+ `sequence-svg.js`) | M | mod bundles only pure code and drops three fake-`document` parameters; `architecture.js` drops to about 1,270 lines; layers become expressible | low (pure moves; wide test-harness edits) |
| 3 | `web/pele-svg.js`: shared SVG admission, `unsupported`, `encode`, theme, `asButton`, measurer | M | one copy of the security-relevant admission code instead of four; fixes the pre-parse inconsistency; about 25 cognitive points off each `validatedSvg` | medium: admission must reject exactly what it rejects now; mutation tests exist |
| 4 | `architecture-pele.js` `route` -> pure `architecture-routing.js` with plan and path tables | M-L | 196 -> about 30; geometry testable in node; `channelReserve` cannot drift | medium-high (visual) |
| 5 | `architecture-pele.js` `render`: `fitLayout` + decorator table + `checkpoint` | M | 252 -> about 25 | medium |
| 6 | Decide and, if confirmed, drop the non-display mode; then split `project` into validate and emit | S decision + M | 166 -> about 20-30; removes about 20 lines of `validatedSvg`; rewrites about 19 test calls | medium; needs the mod's options checked |
| 7 | `sequence-pele.js`: `visit`, `add`, `render`, `validatedSvg` helpers | M | 165/60/90/83 -> at most 25 | medium (statement-id order), strongly pinned |
| 8 | Five small wins: `inspect` table, `closureNames`, `navigation.open` pure parts, `treemap-pele` `project`/`validatedSvg`, `setSection` | 5 x S | five functions over 55 brought to 25 or less | low |
| 9 | `app.js` split per section 5, then `readNavigationLocation` and `refreshStatus` | M-L | 3,179 -> under 600 per file; three functions over 60 fixed | low for moves, medium-high for `refreshStatus` |
| 10 | Shared `sourceWitness`, DOM `make`, context menu out of `classes.js` | S | 5 -> 1 witness copies; ends the global collision; removes a peer dependency | low |
| 11 | Mod: split `main.js` `Trellis` class into checkout/status, architecture, deviations and sequence controllers; `deviations()` row builders; DTO validators shared from web | M | 988-line class broken up; `client.js` copies and drift removed | low-medium |
| 12 | `rules.toml`: add the three `herdr-mod` layers now; add `web` layers when references can see globals or after ESM | S | makes the mod's check non-vacuous | none |

Suggested order: 1, 10, 3, 2, 8, then 4-7 one adapter at a time, then 9 and 11. Item 1 and item 2 come first because they lower the cost of everything after them.

**Redesign overlap (#303).** Slices #304-#311 are closed, so Architecture, Treemap, Files, Connect and the shell are already the redesigned versions. Remaining overlap:
- **#312 remote daemons over SSH (open)** will change the checkout list, connect flow and probably the API client: `app.js` 876-1192 (`loadCheckouts`, `selectCheckout`, connect handlers), `shell.js` `checkoutLabel`/`updateCheckout`, `herdr-mod/src/client.js`. Do not polish these before it lands; `app-checkout.js` should be the last piece of item 9.
- **#512 class relationships (open)** will rewrite `classes.js` `project` and `validatedSvg`. Do only the shared-admission part (item 3) there.
- **With-Herdr shell, Handoff, C4**: not scheduled; they add new code and replace nothing here.
- #594 (Sequence default) is closed.

**Leave alone:**
- `web/vendor/**`.
- `herdr-mod/src/dom.js` `parseXml` and `diagram.js` `segments`: compact parsers, churn 2-4, bounded inputs.
- `metric-heat.js`, `architecture-apps.js`, `files.js`: small, pure or nearly so, low churn, no function over 42.
- `treemap.js` and `viewer-metrics.js` controllers: one function over 50 between them (`treemap.js:165` `render`, 61).
- The LOC of IIFE wrappers and `create()` factories (for example `architecture.js:380`, 1,264 lines, cognitive 3): an artefact of the module pattern. Exclude them from function-size targets; file-size targets cover them.
- Long flat `||` validation conditions in the adapters: they score high but read as allowlists. Split by responsibility, not to move a number.
- `sequence.js` `drawSequence`: no decomposition until the fallback's future is decided.
- `drawApps` (`architecture.js:888`): changed on 2026-11-12; revisit after it settles.
- ES-module migration of `web/`: large, touches 17 test harnesses and the mod wrapper, and buys nothing the classic-script splits do not, apart from rules visibility.

---

## 7. Proposed measurable targets

Current counts are production declarations from `decls.json`.

| Target | Catches today (web) | Catches today (herdr-mod) | Note |
| --- | --- | --- | --- |
| No function with cognitive >= 100 | 5 | 0 | first milestone: items 4-7 |
| No function with cognitive >= 60 | 23 | 1 (`parseXml`, allowlist it) | end-of-programme goal |
| Ceiling for new or changed functions: cognitive < 40 | 42 | 3 | gate on the diff, not the backlog |
| Stretch: cognitive < 25 | 79 | 12 | not a gate |
| Function body <= 100 lines, excluding IIFE wrappers and `create`-style factories | 11 of the 29 at >= 100 | 0 | the 11: `drawSequence` 339, arch-pele `project` 198, seq-pele `project` 185, `route` 156, arch-pele `render` 155, seq-pele `render` 133, `draw` 118, `drawApps` 109, `readNavigationLocation` 107, `architecture.js` `render` 102, `navigation.open` 100 |
| maxNesting <= 5 | 18 at >= 6 | 6 at >= 6 | the metric counts `else if` chain length (see Tool gaps); review hits by hand |
| File <= 1,000 lines | 2 (`app.js` 3,179, `architecture.js` 1,645) | 0 (`main.js` 988 is at the edge) | |
| File cognitive sum <= 1,000 | 3 (`app.js` 2,199, `architecture.js` 1,756, `architecture-pele.js` 1,330) | 0 | |
| Copies of SVG admission prefix | 4 -> 1 | - | |
| Copies of `sourceWitness`/`witness` (full form) | 5 -> 1 | - | |
| References from lower layers to `app.js`-defined globals | 36 -> 0 | - | |
| Web files the mod loads with a `document` it only needs for unrelated controller code | - | 3 -> 0 | `architecture`, `architecture-deviations`, `sequence` |
| Mod copies of web DTO helpers | - | 5 -> 0 | `pinOf` group, `indexLabel`, `jobCause`, `architecturePayload`, `detailPage` |

---

## 8. Tool gaps (Trellis MCP)

1. **`trellis_references` cannot see `web/`'s structure.** `prefix:"web"`, `groupBy:"file"` returned 16 modules, `groups: []`, `mappedSites: 0`, tier `syntax`. All coupling is through `window.Trellis*` globals and injected callbacks. I built the dependency matrix with `grep` instead. This is also why intra-app rules for `web/` would be vacuous.
2. **`trellis_references` with `prefix:"herdr-mod"` failed**: `store_unavailable: Store is unavailable`. Not retried. So I could not check whether `trellis-web:<name>` virtual imports resolve to `web/` files.
3. **`trellis_outgoing_calls` failed** with `store_unavailable` on `web/architecture-pele.js` `validatedSvg` (`sid:v1:554e421d...`). Same failure class as the workbench README's gap A1 (#595).
4. **JS semantic evidence was not usable.** `find_symbols` and `incoming_calls` reported scip-typescript as `available:false`, `freshness:"possiblyStale"`; `stableLookupKey` was `null` for all four `validatedSvg` declarations.
5. **`trellis_incoming_calls` by name merges same-name declarations.** For one `validatedSvg` it returned callers of all four (`sameNameDeclarations: 4`), each `boundaryReason:"stale"`, and the test callers as `unresolved`. Useful as a grep, not as proof of which adapter a test pins.
6. **`compact:true` is still large**: that answer was about 17 KB, much of it `scopeCoverage` rows for Java, Rust, Python and TypeScript that have nothing to do with a JavaScript symbol.
7. **No duplicate-code or same-name-collision query.** The `sourceWitness` double declaration across two classic scripts, and the four-way admission prefix, were found by reading and `grep`. A "same name declared at top level in two scripts of one page" check would have caught a real hazard.
8. **`maxNesting` counts `else if` chain length.** `architecture.js:1281` `inspect` scores 12 and `herdr-mod/src/main.js:543` `press` scores 12 for flat chains at real depth 2-3; `architecture-pele.js:665` `render` scores 17 at real depth about 6. A nesting ceiling needs manual review until this is separated.
9. **Closures dominate the JS declaration list.** 516 "declarations" in `app.js` include one-line arrow callbacks (`() => perform(refreshView, ...)`); 284 are top-level. IIFE wrappers and factories appear as 1,000-line functions with cognitive 0. A way to filter to named, non-wrapper functions would make the baseline easier to read.
10. No tool answers "which tests exercise this function" for code loaded through `vm.runInNewContext(fs.readFileSync(...))` or the mod's `trellis-web:` loader; test pinning in section 4 is by file-name grep.

I used `trellis_find_symbols`, `trellis_incoming_calls` and `trellis_references` (three calls answered, two failed), then fell back to `Read`, `grep` and the metrics JSON. `trellis_read_source` and `trellis_call_graph` were not exercised.
