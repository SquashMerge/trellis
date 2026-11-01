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

Re-dogfooded on 2026-10-31 (second pass) at `944417ad` (index revision `57cfcbba:6`), 1440×900, in the browser, against the daemon's API responses and through the Trellis MCP tools. The owner rulings below win where they differ.

**Shipped since the first 2026-10-31 review (`cbb32684`):**

- **Sequence participants and incidental calls.** Sequence already enriched exact semantic targets before this pass (#521's correction).
  - Fresh rust-analyzer call bindings select measured declaration participants: `coordinate` has 10 participants, up from 1.
  - External calls (the standard library and other packages), logging calls and trivial accessors are hidden behind a "Show N incidental calls" toggle. N counts incidental steps, operands included, so it isn't necessarily that many calls (64 for `coordinate`).
- **Semantic evidence in Architecture.**
  - 15,259 of 50,561 reference sites now map to a module (30%, up from 9%).
  - Deviations went from 24 to 35 as more code became visible.
  - MCP `rules_check` now counts `semanticBinding` evidence.
- **Tooling.** With tooling in a `display = "hidden"` layer, as in this checkout's local rules: out of the layer stack until the hidden-layer toggle reveals it, grey in the treemap, and out of the Sequence start list. The start list is application code only.
- **Architecture text.** Coverage appears once (in the legend); names wrap instead of truncating.
- **Files.** The outline shows types and callables, with members under an expanded type. The inspector caveat is behind an ⓘ. Source-margin labels no longer include IDs.
- **Treemap.** The "Refresh treemap" and "All indexed files" buttons are gone.
- **Load.** `/dependencies` no longer loads at startup; it loads on the first in-context library action. Pinned reads check the head through a small `/status/head` response; checkout selection and refresh still read full `/status`.
- **MCP.**
  - `workspace_describe` `languages` and the find, search and incoming `scopeCoverage` report a per-language `semantic` block (rust-analyzer, scip-typescript).
  - `find_symbols` omits its `syntaxOnly` warning when every language in the answer has fresh selected semantic evidence. The warning stays otherwise.
  - Pages target 24 KiB (`pageBytes` = 24,576 bytes). That isn't a hard cap: a lone larger item still gets its own page under the unchanged 65,536-byte response cap.

**Retained, not new:** Rust support in the Classes tab (`src/classes/rust.rs`) predates the first review (#452, #506).

Items are ordered by impact.

**Corrections after review (2026-10-31, Prime's review of PR #561):**

- **Shipped list:** Sequence's semantic enrichment and Rust Classes predate this pass, and the toggle, `syntaxOnly`, `pageBytes` and `/status/head` bullets were too broad. Reworded above.
- **Item 1 (load):** the timings stand, but the cause isn't profiled. Direct metrics doesn't run the Architecture semantic join, and a source-score memo already exists.
- **Item 2 (`call_graph`):** the boundary is the ratified #470 rule, and free-function calls already expand. The owner has since ruled on #557 to amend it.
- **Item 5 (Classes):** members exist and start collapsed. Proven associations are a new evidence feature, not edges waiting to be drawn.
- **Item 7 (rules):** resolved in the owner's local, untracked `.trellis/rules.toml`.
- **Item 10 (MCP):** the `syntax` tier describes the native answer by contract (#528). The `semantic` block reports availability beside it.
- **Item 11 (`rules_check`):** the full-snapshot `unassigned` catalog is contract behaviour, not an ignored filter. Whether to change it is an open question on #559.
- **Tooling:** hiding it was already settled by the #524 ruling (`docs/rules.md`).

### A. Blockers

1. **The landing view times out.**
   - `GET …/architecture` takes 17–20 s; the client gives up at 15 s and shows "Request timed out. No automatic retry was made."
   - The second attempt loaded. Whether a warm cache explains that wasn't measured.
   - Metrics went from 2.3 s to 8.9 s.
   - MCP `rules_check` on `src/store.rs` fails with `deadline_exceeded` (5 s) twice in a row.
   - The cause isn't profiled yet. The semantic join is a hypothesis for Architecture and `rules_check`, which share it; direct metrics doesn't run it.
   - **Proposed (#556):** compute the architecture and metrics summaries once per index revision and serve them from a cache. Don't just raise the client timeout. This goes beyond the existing source-score memo, has to respect live rules-file changes and request options, and has to be measured separately for MCP `rules_check`.
2. **`call_graph` stops at this seed's method calls.**
   - `Store::navigation_at → navigation_in` binds `resolution: resolved`, `join.status: exact`, yet the edge returns `to: null`, `visit: boundary`, `boundaryReason: dispatch`. The binding's `dispatch` is `unknown`.
   - That's the ratified #470 rule: an exact binding doesn't prove dispatch, and member calls and Rust method targets stay `unknown`. So depth 2 returns only the root node here. Fresh exact bindings to free functions already expand.
   - **Approved amendment, in progress (#557, owner ruling 2026-10-31):** a fresh exact binding to a fixed body is direct and expands: Rust inherent methods, and Java static, private and final methods. Rust trait methods and JS and Java instance methods stay dynamic; the edge shows the trait, interface or prototype declaration as its target, marked dynamic, rather than a target-less boundary, with no fan-out to implementations. `impl Trait for Type` is measured on this repo first.

### B. Sequence and Classes

3. **Large sequences are unreadable.**
   - `coordinate` renders at 4680×14269px, scaled to 25% (12px text shows as about 3px).
   - Each callee method gets its own lifeline (`receive`, `decode`, `start_final_check`, `phase_hook`, …), where the design uses one per type or module.
   - Repeated `?` frames ("only if the preceding path continues normally") add height.
   - **Fix:** group lifelines by type or module, collapse `?` continuation frames into a marker on the call, and open at 100% with scrolling (plus a fit toggle) instead of fit-to-width.
4. **Empty sequences after hiding.** `witness` has only two std calls, so with them hidden the diagram is a single box. When every step is incidental, show them, or say "Only standard-library calls (2)" with the toggle.
5. **Rust class boxes open empty.**
   - `WatcherHealth` shows "3 fields · 7 methods" with empty compartments and "No proven class relationships".
   - The members are there, collapsed by default: **Show members** in the class's action menu renders them.
   - "No proven class relationships" is deliberate. Field and signature types, e.g. `witness() -> WatcherWitness`, are kept as syntax hints, and a producer knowing a return type doesn't by itself establish an association edge.
   - **Proposed:** show members by default. Separately, add proven return-type and field-type associations, a new evidence feature that needs its own contract path.
   - The Classes tab also appears a moment after the other tabs (after the `classes?limit=1` probe), so decide the tab set before the first render.

### C. Architecture

6. **Arrows cross the boxes.**
   - Arrow count tags ("↑≥150 · 32% mapped", "↑≥1 · 26% mapped") sit on the borders between bands.
   - The red upward arrows from storage run diagonally through module boxes (`metrics`, "+19 more").
   - **Fix:** route the upward arrows in a right-hand channel, as the mockup does, with their tags on the channel.
7. **New modules were unassigned (resolved locally).**
   - Unassigned grew from 275 to 283 in this pass.
   - `.trellis/rules.toml` didn't cover `src/producer/**`, `src/architecture_details.rs`, `src/behavior_incidental.rs`, `src/classes/rust.rs`, `src/graph_query.rs`, `src/lib.rs` or `src/native_rust_macros.rs`.
   - **Resolved:** the owner has since assigned them in the local `.trellis/rules.toml`, which isn't tracked. No rules PR is pending.

### D. Files and Treemap

8. **Outline names are cut to ~5 characters** ("relev…", "certa…", "read_…"), because the "function L170" suffix takes the width. Use a kind icon, put the line number on hover, and let names take the row.
9. **Treemap is still mostly orange.** The default `max` aggregation puts most files in the top fixed band (#395). Consider `mean` as the treemap default; keep the fixed bands.

### E. MCP

10. **The top-level evidence label still says "syntax".** `evidenceTier` is `syntax` for every language (and on each symbol), even where the new `semantic` block reports a fresh producer. That's by contract (#528): the tier describes the native answer, and `semantic` reports what's available beside it. Agents read the top-level field first, so the distinction may need clarifying.
11. **`rules_check` keeps the full `unassigned` catalog under `paths`.** A check scoped to `src/watch.rs` returned 59 unrelated unassigned files (`tests/*.cjs`, `src/producer/*`). That's the current contract: `paths` scopes source evaluation, while the informational `unassigned` catalog uses the full snapshot. Scoping the list to the requested paths would be a contract change, an open owner question on #559.

## Suggested slices (each its own issue)

**Already in the running app (2026-10-31, second pass).** These were the original slices 1–6, the 2026-10-29 slices 1–5, and most of the first 2026-10-31 slices:

- the brand restyle and Connect
- the simplified shell, Architecture and Treemap with metrics, and Files
- the Architecture payload, and tabs without Libraries
- semantic bindings in Sequence and Architecture
- application layers only
- the Architecture text pass
- Files and Inspector
- the Treemap buttons
- MCP per-language semantic reporting

Confirm that `DESIGN.md` and `docs/design-refresh-contract.md` were amended along with the restyle.

Item numbers in brackets refer to the gap list above.

### Next

1. **Architecture and metrics per revision** [1]. Proposed in #556.
   - Profile first. Compute once per index revision and serve from a cache, beyond the existing source-score memo, respecting rules-file changes and request options.
   - The landing view loads within the client timeout; MCP `rules_check` meets its deadline. Measure each separately.
2. **Static calls expand in `call_graph`** [2]. Approved amendment to #470, in progress (#557): fresh exact bindings to fixed bodies (Rust inherent methods; Java static, private and final methods) expand; dynamic calls show the trait, interface or prototype declaration as their target.
3. **Readable sequences** [3, 4].
   - Lifelines per type or module; `?` frames collapsed.
   - Opens at 100% with scrolling.
   - An all-incidental sequence still shows something.
4. **Rust class diagrams** [5]. Members shown by default; the tab set is decided before the first render. Proven type associations are a separate evidence feature.
5. **Architecture arrows** [6]. Upward arrows in a side channel.
6. **Files outline and Treemap default** [8, 9].
7. **MCP clarity** [10, 11]. Clarify that the `syntax` tier describes the native answer, beside the per-language `semantic` block. Scoping the `rules_check` unassigned catalog to `paths` is a contract change, only if the owner approves it on #559.
8. **Remaining diagram views.** C4. No ERD.

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

- **`rules_check` unassigned catalog:** keep the full-snapshot `unassigned` list under `paths` (current contract), or scope it? Open on #559.

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
