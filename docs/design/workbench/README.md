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

Reviewed in the running app at `localhost:8877` on 2026-10-29 (`6925a2c`, index revision `d192e32e:2`) at 1440×900. Each finding was checked against the daemon's own API responses and the Trellis MCP tools. The owner rulings below win where they differ, except where an item says it proposes changing one.

**Already fixed since 2026-10-28:**

- one header row with a single status
- the checkout picker shows `repo ⎇ … · rev`
- fixed per-metric color bands (#395)
- fixtures and vendored code excluded from heat
- Architecture folded to deviations, with ↑ counts
- the treemap root is named `trellis`
- one-line symbol rows
- the Connect logo

### A. Simplify: the screens carry too much

The app shows every control and every caveat at once. The rule for the next pass: **one row of controls per view, one status in the header, caveats behind an ⓘ, and every tab has something to show on arrival.**

1. **Open straight into a diagram.**
   - After Connect, select a checkout automatically: the only one, or the one last used, or the one matching the agent's cwd.
   - Land on **Architecture**, not an empty Sequence.
   - The picker stays in the header for switching.
2. **Header: picker, status chip, menu.**
   - The status chip shows "Fresh · rev 2". A failed re-index is a secondary note on the chip, and its menu holds Refresh and Diagnostics.
   - Show the amber **Index workspace** button only when there's no usable index.
   - Move Disconnect into the menu.
3. **Left rail: just the file tree.** Remove:
   - The **Indexed checkouts** card. It repeats the header picker; bring it back only when there are 2+ checkouts, as the picker's dropdown.
   - "Agents can use these indexes through trellis mcp", the absolute path, the "Index scope & file access" disclosure and the "Expand folders to browse…" paragraph.
   - The "Filter loaded entries" label. The tall filter box becomes a one-line filter that searches the whole tree, not just loaded folders. Today, filtering `src/watch.rs` on the Diagrams page finds nothing, because unopened folders aren't searched.
   - The rail legend. It repeats the diagram legend; show it only as a tooltip on a swatch.
   - Show hidden/ignored and Display options move into a ⋯ menu.
4. **View toolbars: one row each.**
   - Keep Color by and the aggregation select (max / sum / mean).
   - Move the checkboxes into an **Options** popover: include tests and fixtures, relative, test-only edges. ("Deviations only" was removed by #449: the full stack is always drawn with deviations highlighted, item 11.)
   - Replace the two or three lines of mono explanation under the toolbar (band names, "20025 callables excluded…", excluded-file counts) with the legend swatches and an ⓘ that holds the detail.
   - Remove **Color by** from Sequence and Classes, where it has nothing to color ("No measured values").
5. **Architecture header.**
   - Turn "Review deviations / Unassigned files / Authored exceptions" into counted chips in the inspector: **23 deviations · 156 unassigned · 0 exceptions**.
   - Remove "Refresh architecture"; the view refreshes on a new revision.
   - Cut "503 native file modules · 431 production / 331 test-only…" and "Showing 3 layers and 25 files…" to one line, or move them under ⓘ.
6. **Only show tabs that can show something.**
   - **Classes** appears only when the checkout has production files in a class-catalog language. `GET /api/classes` reports those as `supportedLanguages` (Java, Python, JavaScript and Rust since #452). Before #452, class diagrams covered only Java and Python, which trellis has only in fixtures and tools.
   - **Sequence** opens from a method: click it in the tree, the source margin or the inspector.
   - With no method selected, the Sequence tab shows a short list of the most complex methods to start from, instead of "Expand a file and select a method."
7. **Libraries leaves the tab row.** *This proposes changing the 2026-10-24 ruling, which kept it as a tab.* Today it's a catalog: 376 crates and 50,000 declarations with no link to this code.
   - Reach it in context instead: "open in library" from a call or reference that lands in an external crate.
   - Optionally, later: a **Dependencies** view showing which external packages each layer uses. The architecture payload already has 51 of them.
8. **Explanatory text, once per view.** The "not a runtime trace / targets unverified / candidates do not prove dispatch" sentences currently repeat in the toolbar, canvas, inspector and footer. Keep one ⓘ "About this view" per view, plus the footer's Privacy & evidence limits.

### B. Fixes found with live data

9. **The status says "Index failed" while the index is fine.**
   - `/status` and MCP `workspace_describe` both report revision 2 as ready and fresh.
   - What failed was the latest re-index job (`/jobs/current`), whose only error is the generic `"Index job failed"`.
   - Show it as a note with the real cause (see item 2).
10. **Architecture downloads 35 MB.**
    - Drawing needs `edges` and `modules`, about 0.7 MB.
    - The rest is undrawn detail: `references.unresolved` (32,472 sites, 15 MB), `rules.testOnly` (11.6 MB) and `rules.instances` (6 MB).
    - The same payload makes MCP `rules_check` and `references` fail with `response_too_large`, even when limited to one file.
    - Fix: return summary data by default, with details per edge or file on demand, and apply MCP `paths` before the size check.
11. **The entry layer vanishes.**
    - It has 36 modules and 172 production references, all downward and allowed.
    - With "Deviations only" it's neither drawn nor listed under "quiet layers", which names tooling and web.
    - Fix: draw the full stack (web, entry, query, indexing, storage, with Shared beside it). Dim the allowed arrows and highlight the deviations, rather than dropping layers.
12. **Architecture leaves most of the panel empty.** The stack is a ~430px column inside a 1196px panel. Use the width, and put module chips colored by metric where it now says "N files folded".
13. **Show coverage next to the arrows.**
    - Only 3,034 of 35,506 reference sites mapped to a module (about 9%). The rest are mostly `rust_module_ownership_unproved` (16,104) and `rust_export_name_set_unknown` (13,700).
    - 563 of 897 rule instances are unknown.
    - So ↑ counts are lower bounds: storage → indexing shows ↑102, while #163's scan found 183. Say "≥102 · 9% of sites mapped".
14. **Arrow legend.** Four lines of prose become a compact symbol key.
15. **Treemap fits its panel.** The controls take about 325px of height, leaving the map 400px of the 712px panel, and it's 69px too wide, so it scrolls sideways.
16. **"Open full" opens the wrong tab.** In Files → Inspector, "Open full" sets `#sequence?…` but shows the last-used diagram tab (Treemap).
17. **The inspector mini-sequence is clipped.** Pele draws it at 720×374 in a 251px-wide box. Scale it to fit.
18. **The selected method is a heavy purple slab.** Use a raised background with a lavender left bar for the selected source range.
19. **References in the inspector.** The design lists "References to WatcherWitness · 4", and the data exists; the live inspector doesn't show it.
20. **IDs in accessible labels.** The source-margin buttons are labelled "Sequence for witness · syntax · sid:v1:…". Drop the ID.

### C. Data limits that shape the design

- **No calls are resolved.** The index resolves 0 of 199,075 calls. MCP `outgoing_calls` for `Store::navigation_at` returns one unresolved call with a same-file candidate, `navigation_in`.
  - That's why the `witness` sequence is a single lifeline with two "call load · target unverified" boxes.
  - The Sequence and Class mockups, which draw resolved participants, overstate what's available.
  - Until resolution lands (#40/#68), draw **native candidates** as dashed participants, labelled as candidates.
- **Rust coverage is partial.** 260 of 274 Rust files are partial, because macro and attribute bodies aren't measured. This belongs in the ⓘ for every view, not in each one's body.

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

1. **Simplify the shell** [1–4, 8, 9].
   - Auto-select the checkout and land on Architecture.
   - Header: picker, status chip and menu.
   - Left rail: the file tree with a whole-tree filter.
   - View toolbars: one row each, with an Options popover and an ⓘ for caveats.
2. **Architecture payload** [10].
   - Summary by default, details on demand.
   - MCP `paths` applied before the size check.
   - Unblocks the MCP architecture tools.
3. **Architecture drawing** [5, 11–14].
   - The full layer stack with Shared beside it, and allowed arrows dimmed.
   - Module chips colored by metric, using the full width.
   - Coverage shown next to the arrows, and a compact legend.
   - Counted chips for deviations, unassigned files and exceptions.
4. **Tabs that earn their place** [6, 7].
   - Classes only for production Java/Python.
   - Sequence opens from a method, with a "most complex methods" starting list.
   - Libraries reached in context (needs an owner decision: it reverses the 10-24 ruling).
5. **Files and Inspector** [15–20].
   - Treemap fits its panel; "Open full" opens Sequence.
   - Mini-sequence scaled to fit; lighter selection.
   - References list in the inspector; IDs out of accessible labels.
6. **Candidate participants in Sequence and Class** [C]. Draw native candidates as dashed participants until call resolution lands.
7. **Remaining diagram views.** C4, and Pele parity for Sequence and Class. No ERD.

### Gated (don't build until the prerequisite lands)

8. **Remote machines over SSH.** Needs the topology amendment (`docs/local-topology.md`, CLAUDE.md "loopback only").
9. **With-Herdr shell.** Comes after the Herdr-client feasibility probe and the terminal contract's transport validation.
10. **Design handoff.** Only once SPEC §11 is scheduled with product criteria. It sends a typed change request from a structural comparison.

## Open questions for the owner

- ~~Where does the layer map live?~~ Settled: `.trellis/rules.toml` (#129), proposed by the agent, approved by PR.
- ~~Should the metric color ramp be quartiles of the visible set (as drawn) or fixed thresholds?~~ Settled 2026-10-28: fixed per-metric thresholds (#395). The mockup's quartile coloring is superseded.
- ~~In Without-Herdr mode, what does "Design a change" produce?~~ Settled 2026-10-29: **copy to clipboard**. The copied text carries the context (checkout, revision, the diagram and selection it came from) and a **structured change request** (the typed change from the structural comparison, per the design-handoff ruling), not a Mermaid text diff. It is not saved as a request for `trellis mcp`.
- ~~How does "Follow active tab" behave when the tab's cwd isn't an indexed checkout?~~ Settled 2026-10-29: tabs whose cwd has **no Trellis index are shown greyed out and can't be clicked through to**. Following such a tab doesn't switch the view.

- ~~**Libraries tab:** keep it as a tab (2026-10-24 ruling), or reach it from external calls and references only (gap item 7)?~~ Settled 2026-10-29: **remove the tab**; reach the catalog in context ("open in library" from a call or reference into an external package). Built in #450.

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
- **Tools:** retired. **Libraries:** kept as a diagram-row tab (`#Libraries`). *Superseded 2026-10-29: Libraries left the tab row and opens in context (#450).*
- **Mockup update (2026-10-24):** the boards above now show these rulings: numbered layers with a peer pair and the Shared box (Architecture, Viewer), Git state (Viewer, Main, Files), machines grouped with SSH machines and an unreachable one (Viewer, Main), the Files working-tree diff, the Libraries board, Later badges, and no ERD.
