# Trellis Workbench — design packet

A design handoff for the next Trellis web app. It is an **interactive mockup**, not an implementation and not evidence that any capability exists. Repo context: `squashmerge/trellis` at `0acaba1`.

Start with `trellis-workbench.html`. Then read `SCREENS.md`, then this file's rules before writing code.

## What's in here

| Path | What it is |
| --- | --- |
| `trellis-workbench.html` | Self-contained interactive mockup of every screen. Open it in a browser. Use the top bar or `#Connect`, `#Main`, `#Viewer`, `#Files`, `#Handoff`, `#Architecture`, `#Treemap`, `#Sequence`, `#Class`, `#Libraries`, `#C4`. Clicks, tabs, selects and toggles work. |
| `SCREENS.md` | Per-screen spec: purpose, layout, states, interactions and the data each one needs. |
| `KICKOFF.md` | A prompt to paste into Claude Code to start the work. |
| `screens/*.png` | 1440-wide screenshots of every screen, plus extra states (Connect → choose mode, Files → working tree, Handoff → verify). |
| `reference/*.dc.html` | Source of each screen: plain HTML/inline-style markup with `{{hole}}` bindings, `<sc-for>`/`<sc-if>` blocks and a small `Component.renderVals()` class holding the mock data and state. Use it for exact measurements, copy and mock data. **It is not code to port.** `canvas.json` lists the boards. |
| `brand/tokens.css` | Every color, type, spacing and radius token the mockup uses (SquashMerge design language v1 / Brand Guide v1.1). |
| `brand/trellis-logo-dark-v1.png` | Official full lockup (dark), 1254². |
| `brand/trellis-mark-transparent-v1.png` | Header symbol, 102×128, transparent. **Provisional, unreviewed**: cropped from the lockup and keyed off black. Replace it when an official compact mark exists. |

## Screens at a glance

There are two app modes and one shared diagram set:

- **With Herdr** (`Main`): Herdr-style sidebar (SSH machines → repos; grouped by repo only, not by worktree). One agent tab is visible at a time (Claude / Prime / Prime2 / Gotop). The Trellis pane follows the active tab's checkout or worktree.
- **Without Herdr** (`Viewer`): indexed-checkouts list grouped by machine (this Mac plus saved SSH machines) with Git state, and file tree, with the diagram pane and an inspector. No terminal.
- **Shared**:
  - `Connect` (token sign-in, then choose mode)
  - `Files` (source view)
  - `Handoff` (design-first change request: **planned**, SPEC §11, not scheduled)
  - Diagram views: Architecture, Treemap, Sequence, Class, Libraries, C4. No Tools tab (retired); no ERD (dropped; a live-database ERD is future work).

## Rules for implementing this (from the repo; they win over the mockup)

1. **Read `CLAUDE.md`, `AGENTS.md`, `SPEC.md` §9 and §11, `DESIGN.md`, and `docs/terminal-workbench-contract.md` first.** If the mockup and a ratified contract disagree, the contract wins. Flag the conflict; don't silently pick.
2. **Brand: already amended.** `DESIGN.md` and `docs/design-refresh-contract.md` carry the owner-approved SquashMerge brand amendment (commit `85c027d`; Inter, Manrope, JetBrains Mono per `brand/tokens.css`), and the running workbench is restyled. Those two documents are the authority; this packet's brand notes are background.
3. **No placeholder capabilities.** Per `DESIGN.md`, don't ship controls that imply something works when it doesn't. That covers three things:
   - Herdr attach, embedded agent terminals and the design handoff are each **gated on their own contract and acceptance**.
   - Build them only when that slice is scheduled.
   - Until then, hide them; never show a disabled fake.
4. **Stack stays as is.** Vanilla JS/CSS in `web/`, no framework, no CDN.
   - Fonts are served locally from `web/fonts/` with their OFL licenses (already done with the brand amendment).
   - The mockup's Google Fonts link is a mockup convenience only.
   - Keep the existing Host/Origin/CSP checks.
5. **Diagrams render as Mermaid → Pele** using the vendored renderer in `web/vendor/` (currently `pele-1.10.0-trellis.2.js`; see `web/vendor/README.md` and `docs/semantic-evidence/decisions/0014-pele-structure-rendering.md`). The mockup's "Pele 1.6.0" labels are historical.
   - Project each view from its measured, revision-bound DTO. Map rendered elements back to Trellis IDs, never Pele's `data-id`.
   - Don't infer anything the model doesn't contain.
   - Respect the diagram size cap in SPEC §9.
   - The mockup's hand-drawn diagrams show intent, not layout.
6. **Repository text is untrusted.** Render labels as text (`textContent`), never as HTML.

## Data in the mockup is illustrative

- **Complexity values:** an approximate scan at `0acaba1`, not Trellis output. The real view reads `metrics:v1` (`docs/metrics.md`):
  - Metrics: cyclomatic, cognitive, maxNesting, maintainabilityIndex, halstead volume, fanOut.
  - Aggregations: max / sum / mean.
  - Production files only.
- **Prod/test edge counts between layers:** Trellis issue #163.
  - Production upward references: store→services 69, store→indexing 183, watch→daemon 1, model→indexing 2.
  - Test-only seams (dashed, toggleable): store→mcp 6, index_coordinator→daemon 4.
- **Layer assignment (#163):**

  | Layer | Modules |
  | --- | --- |
  | UI | web |
  | Entry | main, http, daemon, mcp, acp, auth |
  | Services | navigation, classes, class_diagram, planning, metrics, behavior*, dependencies*, file_tree, jev, live_jev, answer, rust_sources |
  | Indexing | indexer*, index_coordinator, capture, watch, native_evidence, native_ids |
  | Storage | store |
  | Shared | model, testing (allowed from anywhere) |

  This lives in `.trellis/rules.toml` (#129): each layer has an `order` number, equal numbers are peers, and the shared layer has no order. The mockup splits Entry into two order-2 peers to show this.
- **Agent transcripts, Herdr machines/tabs and worktree names:** copy only.

## Brand usage notes

- **Status:** always show a word and a shape, never color alone.
  - Fresh / catching up / stale / failed
  - Test-only edges are dashed
  - Diff markers are + − ~
- **Primary action:** amber fill, ink text, 44px minimum height, 8px radius. Use only one per region.
- **Selection:** purple fill, ink text.
- **Links:** lavender, underlined.
- **Focus ring:** 2px lavender, 3px offset.
- **Heat ramp:** line, gold, amber, orange by **fixed per-metric thresholds** (#395; the mockup's quartile binning is superseded), with ink text on warm fills. It reverses for Maintainability. Always show a legend stating the bands.
- **Logo:**
  - The full lockup only appears on Connect, at about 280px of visible artwork with clear space.
  - App headers use the small symbol and the text "Trellis by SquashMerge". The symbol is provisional (see above).
  - No favicon until a reviewed compact mark exists.

## Known gaps in the mockup

- Some in-diagram labels are 11px, below the 12px caption size. The implementation should meet 12px or truncate with a tooltip.
- Architecture: module names under small chips truncate and crowd on the Shared row. C4: a few edge labels collide or clip.
- Layout is desktop 1440×900 only. Small-screen behavior follows `DESIGN.md` (drawers, contained diagram scroll); it isn't drawn here.

## Current UI vs. this design (gap list)

Reviewed in the running app at `localhost:8877` on 2026-10-28, at 1440×900, checkout `trellis` (index revision `937ecdcf:6`). The brand tokens and fonts are already in, along with Architecture, Treemap, color by metric, the indexed-checkouts list and the Files view with its Indexed / Working tree toggle. The items below are what remains. The owner rulings below win where they differ.

### Architecture (largest gap)

1. **One chip per module, not per file.**
   - Today Pele draws every file as a tile: 2191×7676px inside a diagram box about 263px tall, so the layer picture can't be seen.
   - Default to module chips per layer band (`screens/06-architecture.png`). Expand to files on click. Respect the SPEC §9 size cap.
2. **Fixtures and tests are hidden by default.**
   - The `fixture-inputs` layer (order 0, 72 files, all "Unavailable") is the first thing drawn.
   - Hide test and fixture layers unless "Include tests" is on, or show them as one collapsed band.
3. **Layer stats as chips, plus arrows between bands.**
   - Each layer now shows six text lines ("Prod refs down: 0 / up: 0 / Up deviation: 0 / …"), mostly zeros.
   - Show only non-zero counts as compact chips, with upward deviations in the error color (e.g. storage ↑56, indexing ↑7).
   - Draw the arrows between bands with those counts.
4. **Exclude vendored code from metrics.**
   - The web layer's max cognitive (582) is almost certainly the vendored Pele bundle in `web/vendor/`, which also sets the Sequence legend's top bin to ">181".
   - Leave vendored files out of metrics, or at least out of the heat scale.

### Treemap

5. **Root label.** Show the repo name, not `source-set:v1:<uuid>`.
6. **Heat scale.**
   - The quartile bins (≤5, 5–16, 16–44, >44) come from every file, including `tools/` and fixtures, so most of the area lands in the top bin.
   - Compute the bins from the visible files, or default the scope to `src/` and `web/`.
7. **Fit the panel.** The 1170×1170 square sits in a wide, short scroll box. Lay it out to the panel's actual size.

### Chrome and layout

8. **Collapse the stacked rows.**
   - Five rows sit above the content: header, revision strip, "Browse folders…" notice, Workspace toolbar, and the index-job line.
   - They leave the diagram about 30% of the height. Fold them into the header plus one status line.
9. **One readable status.**
   - The header says "Fresh · revision 937ecdcf:6" while the line below shows `Index failed · 0/0 · { "code": "index_failed", … }` as raw JSON.
   - Show a single status (word plus shape), with the details under Diagnostics.
10. **Checkout picker.**
    - It shows the absolute path plus "available", then repeats the path on the line below.
    - Show `repo ⎇ branch · rev`, per `screens/03-viewer.png`.
11. **Explorer.**
    - Replace the per-file "Unsupported source type" rows (`.env`, `Cargo.toml`, …) with one quiet marker.
    - Folder numbers (docs 43, web 582) have no label. Add the metric swatch and a legend.
12. **Sizes.**
    - Body text and buttons are still 12–13px. Workspace buttons are 28px tall with a 4px radius, while header buttons use 8px.
    - Use 14px for labels and an 8px radius on all controls. Keep the 44px height for primary actions only.

### Files

13. **Symbol rows on one line.** Today each is a two-line row, e.g. "variable · MAX_DIRTY_PATHS · L19". Use icon plus name, with kind and line right-aligned (`screens/04-files.png`).
14. **Optional:** subtle syntax coloring, e.g. gold for attributes as in the design.

### Connect

15. **Logo.**
    - The lockup shows a visibly darker box and is about 210px wide in an 800px viewport, below the roughly 260px brand minimum.
    - Crop it with CSS (as in `reference/Connect.dc.html`). Stack the logo above the form on narrow viewports.

## Suggested slices (each its own issue)

**Already in the running app (2026-10-28).** These were slices 1–6 of the original list:

- brand tokens and fonts
- the Connect screen
- the Without-Herdr shell: indexed checkouts, diagram tabs, inspector
- Architecture and Treemap with color by metric
- the Files view with the Indexed / Working tree toggle

Confirm that `DESIGN.md` and `docs/design-refresh-contract.md` were amended along with the restyle. If they weren't, that amendment comes first.

Item numbers in brackets refer to the gap list above.

### Next

1. **Metrics hygiene** [4, 2, 6].
   - Exclude vendored code from metrics and heat scales.
   - Hide test and fixture files and layers unless "Include tests" is on.
   - Use **fixed per-metric thresholds** for heat colors (owner ruling 2026-10-28, #395), not quartiles of the visible set; an optional, clearly labelled "Relative" toggle may show visible-set quartiles.
   - Small change, and it makes every colored view readable.
2. **Architecture at module level** [1, 3].
   - One chip per module in each band; files on click.
   - Non-zero reference counts as chips.
   - Arrows between bands, with upward deviations in the error color.
   - Respect the SPEC §9 size cap.
3. **Page chrome and status** [8, 9, 10, 12].
   - Fold the five stacked rows into the header plus one status line.
   - One readable index status, with raw errors under Diagnostics.
   - Checkout picker as `repo ⎇ branch · rev`.
   - Control sizes and radii per `brand/tokens.css`.
4. **Treemap and explorer polish** [5, 7, 11].
   - Repo name as the treemap root; fill the panel.
   - Collapse the "Unsupported source type" rows into one marker.
   - Explorer metric swatches with a legend.
5. **Files and Connect polish** [13, 14, 15].
   - One-line symbol rows.
   - Optional syntax coloring.
   - Crop and size the Connect lockup, stacking it on narrow screens.
6. **Git state** (owner ruling).
   - New read-only daemon capability: branch, ahead/behind and modified files, measured from Git.
   - Feeds the header, the checkout list and the explorer markers.
7. **Remaining diagram views.** C4, and Pele parity for Sequence and Class. No ERD.

### Gated (don't build until the prerequisite lands)

8. **Remote machines over SSH.** Needs the topology amendment (`docs/local-topology.md`, CLAUDE.md "loopback only").
9. **With-Herdr shell.** Comes after the Herdr-client feasibility probe and the terminal contract's transport validation.
10. **Design handoff.** Only once SPEC §11 is scheduled with product criteria. It sends a typed change request from a structural comparison.

## Open questions for the owner

- ~~Where does the layer map live?~~ Settled: `.trellis/rules.toml` (#129), proposed by the agent, approved by PR.
- ~~Should the metric color ramp be quartiles of the visible set (as drawn) or fixed thresholds?~~ Settled 2026-10-28: fixed per-metric thresholds (#395). The mockup's quartile coloring is superseded.
- In Without-Herdr mode, what does "Design a change" produce: a saved request picked up via `trellis mcp`, or copy-to-clipboard?
- How does the "Follow active tab" toggle in With-Herdr mode behave when the tab's cwd isn't an indexed checkout?

## Owner rulings on the packet (2026-10-24)

These answer the review of this packet against the ratified docs. Where they conflict with the sections above, these win.

- **Git information:** show it where available. That means branch, ahead/behind and modified files, measured from the checkout and never invented. It's a new, read-only daemon capability with its own issue.
- **Working tree (Files):** the "agent edits" side shows uncommitted changes against Git, not unindexed bytes presented as indexed evidence.
- **Remote machines:** Trellis connects to daemons on other machines the way Herdr does, over SSH (`herdr --remote <ssh-target>`, saved machines). Trellis and Herdr then show the same machines. The daemon stays on loopback; the transport is SSH. This needs a topology amendment (`docs/local-topology.md`, CLAUDE.md "loopback only").
- **Herdr embedding:** run a feasibility probe of Trellis acting as a Herdr client, the way the Herdr TUI is one, instead of the per-pane embedding drawn in `Main`.
- **Design handoff copy:** fixed. The agent receives a typed change request from a structural comparison, not a Mermaid text diff.
- **Gated controls** (Design a change, Attach Herdr…): left out until their slices are ready. The mockup keeps them with a dashed **Later** badge.
- **Renderer:** run a Pele 1.6.0 feasibility probe for Architecture and Treemap. The 3D code city and Reagraph graph (#121) are out for now; the 2D treemap stays.
- **Label sizes:** decided when the views are built and laid out.
- **ERD:** the measured-from-`CREATE TABLE` view is dropped. A live ERD from configured databases (SPEC §10) is a future feature.
- **Shared layer:** drawn as a vertical box beside the layer stack, holding utilities and other code usable from any layer. In `.trellis/rules.toml` (#129) it's a layer that any layer may call, with no `order`.
- **Layer order:** each layer carries an explicit `order` number; equal numbers are peers (for example web resources and event listeners both at 1). Calls between peers are deviations unless excepted (proposed default, #129).
- **Tools:** retired. **Libraries:** kept as a diagram-row tab (`#Libraries`).
- **Mockup update (2026-10-24):** the boards above now show these rulings: numbered layers with a peer pair and the Shared box (Architecture, Viewer), Git state (Viewer, Main, Files), machines grouped with SSH machines and an unreachable one (Viewer, Main), the Files working-tree diff, the Libraries board, Later badges, and no ERD.
