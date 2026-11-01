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

Re-tested on 2026-11-01 at `1dffa462` (rust-analyzer with `test-support` and scip-typescript fresh; 53,396 internal call bindings), in the browser and through the Trellis MCP tools from Claude Desktop and Claude Code. Owner rulings win where they differ. Items are ordered by impact; issue numbers are in brackets.

**Fixed since the 2026-10-31 pass (`944417ad`):**

- **Architecture loads:** 0.5 s on first load, down from 17–20 s; metrics 0.2 s, down from 8.9 s. Built once per revision (#556, #571, #587).
- **Coverage:** 47% of reference sites map to a module, up from 30%.
- **MCP `rules_check`:** `src/store.rs` answers within its deadline, and `unassigned` follows `paths`, with a checkout-wide `unassignedFileCount` (#559 option A).
- **Sequence:** one column per module or type (`mcp`, `Session`, `Inbox`), readable at 100% with a "Fit to width" option and sticky column names; Rust `?` continuations collapse; `witness` says "Only standard-library calls (2 hidden). Show them" (#558).
- **Classes:** members show by default (`WatcherHealth`: 3 fields, 7 methods) (#559).
- **Files:** outline names show in full (#559).
- **Call graph (CLI):** `trellis query` from `Store::navigation_at` at depth 2 reaches 10 nodes, including `navigation_in` (#557). Through MCP it's blocked; see A1.
- **Rules file:** the new modules are assigned in `.trellis/rules.toml`.

### A. Blockers

1. **MCP call tools fail [#595].** `outgoing_calls` and `incoming_calls` return `store_unavailable: Store is unavailable` for every symbol tried, repeatably, from fresh MCP processes on the new binary. `call_graph` returned coverage fields its own output schema rejects (Desktop) or `deadline_exceeded` (Claude Code). Find, search, declaration, read and rules work, and the CLI query works, so the fault is in the MCP call-tool path.
2. **Symbol IDs change when code above them moves [#593].** IDs include the enclosing impl block's position (`implementation@44` became `@45`), so an ID saved before an unrelated edit returns `not_found`. Owner decision pending: stable IDs or a stable lookup key.
3. **Search has no model configured [#594].** `search_symbols` falls back to names (`model_not_configured`, 0 of 16,139 embedded); its third hit was an unrelated Python `Summary`. The model is read from the daemon's environment; it should be configured per repository.

### B. MCP

4. **`find_symbols` ranks a test fixture's `navigation_at` above the production method [#594].**
5. **The top-level evidence label still reads "syntax" [#594].** It describes the native answer by contract (#528), beside the per-language `semantic` block, but it keeps reading as wrong. The web source-margin label ("Sequence for witness · syntax") has the same problem. Rename it to say what it describes.
6. **Unassigned counts disagree [#594]:** `rules_check` reports `unassignedFileCount: 164`; the Architecture inspector says 223.

### C. Sequence and Classes

7. **Big sequences are mostly control flow [#594].** `coordinate` hits the 200-step cap at 14,139 px. Most steps aren't calls: 27 continues, 13 nested callable, 9 match, 9 return, 8 write, 7 unsupported attributes. Show calls by default, with control flow behind "Show all steps".
8. **Class diagrams have no relationships yet [#512],** though `witness() : WatcherWitness` is in the box. Long field types overflow the box [#594].

### D. Architecture and Treemap

9. **Arrows cross the boxes [#560].** Red upward arrows from storage cut diagonally through module boxes, including `metrics`; count tags ("↑≥12 · 44% mapped", "↓≥686") sit on layer borders and overlap.
10. **A test fixture sat in Shared.** `src/native_navigation_fixture.rs` was assigned to shared; it's now in tooling in `.trellis/rules.toml`.
11. **Treemap defaults are still cognitive / max**, so it's mostly orange [#560].
12. **The Architecture payload grew to 1.5 MB.** Fast now, but worth watching [#594].

## Suggested slices (each its own issue)

### Next

1. **MCP call tools** [1]: #595, with MCP-level tests that validate every response against the catalog.
2. **Stable symbol references** [2]: #593, after the owner's decision.
3. **Per-repository search model, ranking, labels and counts** [3–6]: #594.
4. **Calls-first Sequence and class box sizing** [7, 8]: #594.
5. **Architecture arrows and Treemap default** [9, 11]: #560.
6. **Proven class relationships** [8]: #512.
7. **Remaining diagram views.** C4. No ERD.

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

- ~~**Tooling layer:** draw `tooling` beside the stack, or hide it by default like `fixture-inputs`?~~ Settled 2026-10-31 (#524, `docs/rules.md`): **hidden**. The layer carries `display = "hidden"` and waits behind the hidden-layer toggle, so the default view depends on that authored setting. *As built in `944417ad`: hidden from the stack, grey in the treemap, and out of the Sequence start list.*

- ~~**`rules_check` unassigned catalog:** keep the full-snapshot `unassigned` list under `paths`, or scope it?~~ Settled 2026-10-31 (#559, option A): **scoped to `paths`**, with a checkout-wide `unassignedFileCount`.

- **Symbol IDs across edits (#593):** must IDs survive unrelated edits, or should tools return a stable lookup key beside them?

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
