# Trellis · apps and deviations — design packet

Design handoff for two Architecture-tab screens. It's an **interactive mockup**, not an implementation and not evidence that any capability exists. Repo context: `squashmerge/trellis` at rev `669eb851`, index revision 43.

- **Apps** (#685): one Architecture tab that shows each app (web app, daemon) with its own layers, and the HTTP interface between them.
- **Deviations**: the review list for layer deviations, drilled down file → function → line. Every line leads to its sequence or its call site.

Start with `trellis-apps-deviations.html`. Then read `SCREENS.md`, then the rules below.

## What's in here

| Path | What it is |
| --- | --- |
| `trellis-apps-deviations.html` | Self-contained interactive mockup. Open it in a browser and switch with the top bar, or go to `#Apps` or `#Findings`. Clicks, toggles, filters and the line menu work. Links to boards outside this packet (Sequence, Files, Classes) do nothing here. |
| `SCREENS.md` | Per-screen spec: purpose, layout, states, interactions, and the data each screen needs. |
| `KICKOFF.md` | A prompt to paste into Claude Code to start the work. |
| `screens/*.png` | 1440-wide screenshots: three Apps states and four Deviations states. |
| `reference/*.dc.html` | Board source: HTML with `{{hole}}` bindings, `<sc-for>`/`<sc-if>` blocks and a `Component.renderVals()` class holding the mock data and state. Use it for exact copy, measurements and mock data. **It is not code to port.** `canvas.json` lists the two boards. |
| `tokens.css` | Brand tokens (SquashMerge design language v1), the same as the workbench packet. |

## Rules for implementing this (from the repo; they win over the mockup)

1. **Read `CLAUDE.md`, `AGENTS.md`, `DESIGN.md`, `docs/rules.md` and `docs/architecture-view.md` first.** Where the mockup and a ratified contract disagree, the contract wins. Flag the conflict; don't silently pick.
2. **The brand amendment comes first.** These boards use the SquashMerge tokens, the same as the workbench packet (slice 1 there). Don't restyle ahead of it.
3. **No placeholder capabilities.** Don't ship a control that implies something works when it doesn't. In particular:
   - **Open in Sequence** needs the Sequence view to accept a highlighted call.
   - **Cross-app sequence** needs #686.

   Hide each one until its dependency exists. Never show a disabled fake.
4. **Stack stays as is.** Vanilla JS/CSS in `web/`, no framework, no CDN, fonts served locally. Keep the existing Host/Origin/CSP checks.
5. **Repository text is untrusted.** Paths, symbol names and source lines render as text (`textContent`), never as HTML.
6. **Don't infer.** Every count, line and verdict comes from a pinned daemon response (index generation + revision + rules hash), never from the working tree. When the basis changes, the view says so and re-reads.

## Data in the mockup is illustrative

- **Real:** file, function and line names come from a scan of the checkout at rev 669eb851:43. So do the layer membership from `.trellis/rules.toml` and the app interface counts (27 matched routes, 34 client call sites, 4 server-only routes).
- **Illustrative:** per-edge totals, per-function counts, the code text on each line, and every "Defined at" line number.
- **Real rows** come from `trellis_rules_check` (one instance per file pair) and `trellis_rules_explain` (up to 50 evidence entries per instance).

## Status of the data path

- `trellis_rules_explain` works when `target` and `destination` are copied verbatim from a `rules_check` instance. That's confirmed from Claude Code.
- From Claude Desktop the same verbatim call still fails with `invalid_request: Invalid tool arguments`. The likely cause is the bare `$ref` with no `"type": "object"`.
- #769 covers three fixes:
  - a ready-made `rules_explain` call on instances where `omittedEvidence > 0`;
  - error messages that name the bad field;
  - `"type": "object"` next to those `$ref`s.

  This screen depends on the first one.
- Today `rules_check` returns one evidence row per pair (#725). That row can be the `use` line rather than the call. For example, for `import.rs → http/worker.rs` it's the module import, not the `enqueue_…` call. So the tree can't be built from `rules_check` alone.

## Suggested slices (each its own issue)

1. **Apps inventory and per-app layers** (#685). Apps come from the build manifests, can be overridden with `[[apps]]` in `rules.toml`, and each app has its own layer order. This slice needs no UI.
2. **Apps view, one app.** The per-app layer stack, the stub on the band that owns the interface, and up/down badges that link to Deviations.
3. **Apps view, all apps.** App columns, the interface edge and its inspector, and the tooling / fixtures / unassigned boxes grouped aside. The edge counts depend on #686.
4. **Deviations tree.** File → function → line from `rules_check` + `rules_explain`, edge chips, Calls only, the selected-line panel, and Go to call site / Go to definition.
5. **The Sequence view accepts a highlighted call.** Opening the caller's sequence with this call marked turns on **Open in Sequence**.
6. **Exceptions from the line menu.** This writes to `rules.toml` per file pair, through the existing rules preview/apply routes.

## Open questions for the owner

- **The tooling chip.** The switcher shows "tooling · hidden", but it can't be selected. Should it be dropped, or should it open the tooling stack? A chip that does nothing breaks the no-placeholder rule.
- **Large files.** `store.rs` has about 500 type-use lines. Should type uses inside a function collapse by referenced symbol (for example "DocumentKey × 36"), with calls always listed line by line?
- **Exception granularity.** Exceptions are per file pair today. Is per caller or per symbol wanted later?
- **Deviations scope.** Is it per app (daemon only, as drawn) or across apps once the web app has rules?
- **`mcp::phase_hook()` from storage** (`mcp_read.rs`, `graph_read.rs`): is this a test hook that should be gated the way `index_coordinator.rs` is?
