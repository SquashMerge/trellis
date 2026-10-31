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

Reviewed in the running app at `localhost:8877` on 2026-10-31 (`cbb32684`, index revision `acb51f94:6`) at 1440×900. Each finding was checked against the daemon's API responses and the Trellis MCP tools after a daemon restart. The owner rulings below win where they differ.

**Shipped since 2026-10-29.** These were the earlier simplification and fix items:

- **Start:** the checkout is selected automatically and the app lands on Architecture. The header is a status chip ("Fresh · rev …") instead of the false "Index failed".
- **Layout:**
  - the left rail is only the file tree, with a whole-tree filter
  - one-row toolbars, with an Options popover and an ⓘ
- **Architecture:**
  - the full layer stack, entry layer included, with Shared beside it
  - inspector chips: 24 deviations · 275 unassigned · 0 exceptions
  - the "% of sites mapped" coverage note and a compact legend
  - the payload went from 35 MB to 844 KB
- **Tabs and Files:**
  - the Libraries tab is removed, with "Open in library" in context
  - the Sequence start list
  - "Open full" opens Sequence
  - the selected source range is raised with a lavender bar
  - references in the inspector ("References to witness · ≥50")
- **MCP:** `rules_check` works, paged at 25 of 259.

Items are ordered by impact.


**Corrections after review (2026-10-31, Prime's review of PR #521; accepted by the owner):**

- **Item 1 (Sequence):** `/sequence` already enriched exact semantic targets. The single lifeline came from the Rust syntax adapter's depth bound, which cut `coordinate`'s loop, and `semantic: unavailable` is display metadata, not enrichment status. Fixed in #531.
- **Item 7 (Classes):** Rust became a supported Classes language with #506, so Classes showing on this repo is correct. The real defect is that `tools/*.py` alone counted. Fixed in #530.
- **Item 12 (MCP):** the `syntax` tier labels describe each native answer (for example, `find_symbols` returns native declarations). The gap is that per-language semantic availability wasn't reported. Addressed in #534.

### A. Data: use the semantic evidence that's already imported

1. **Sequence ignores the semantic bindings.**
   - MCP `outgoing_calls` now resolves calls through rust-analyzer: `Store::navigation_at → navigation_in` is `resolution: resolved`, `evidenceTier: semantic`.
   - But the `/sequence` response still marks the seed `semantic: unavailable` and returns the seed as its only participant.
   - `coordinate` (`src/mcp/mod.rs`, 571 lines) renders as one lifeline with 32 steps. 18 of them are std/core/alloc calls (`Arc::new`, `clone`, `into`), and the view says "0 incidental steps hidden".
   - **Fix:** build participants from the resolved bindings, and hide standard-library calls by default as incidental, with a count and a toggle.
2. **Architecture references ignore the semantic bindings.**
   - Coverage is still 9%: 3,737 of 40,481 reference sites mapped to a module.
   - `rules_check` still lists the limitation "Calls and type relationships have no admitted bindings".
   - **Fix:** feed resolved call and type bindings into references and rules. Arrows and deviation counts stop being "≥" lower bounds where coverage is complete.

### B. Architecture

3. **Tooling reads as the top layer.**
   - `tooling` (order 1, a peer of `web`) sits at the top of the stack, so `tools/` looks like part of the UI.
   - It's developer tooling, not runtime architecture.
   - **Fix:** treat it like `fixture-inputs`: beside the stack or hidden by default (see open questions).
4. **Labels collide and truncate.**
   - Arrow-count tags overlap the band header chips: in the query band, "≥65" sits over "→shared ≥7".
   - "↑≥1 · 7% mapped" runs into the indexing header.
   - The "1 hidden layer" tag floats over the bottom of the drawing.
   - Band titles and chips are cut off ("1 · tooling · 115 files …", "Shared · shar…", "architecture-…", "dependency_…"), because the drawing is 715px wide in a 912px area.
   - **Fix:** route the count tags beside the arrows, wrap or widen labels, and make the hidden-layer note part of the layout.
5. **Repeated text.**
   - The coverage sentence appears twice in the view (above the drawing and under the legend) and again in the inspector.
   - Two lines of counts and bands remain above the drawing.
   - **Fix:** keep the coverage sentence once, next to the arrows or in the inspector, and move the counts into the ⓘ.

### C. Tabs, Sequence and Files

6. **The Sequence start list is mostly dev scripts.** Three of the five most complex methods are `tools/semantic-contract` scripts. Limit the list to application layers, excluding tooling and fixtures, or group it by layer.
7. **Classes shows for a Rust app.** The tab appears because `tools/*.py` counts as production Python. Base the rule on application-layer code, not on any production file.
8. **The file tree lists every member.** The `watch.rs` outline shows fields, parameters and impl blocks (`epoch` field, `bits` parameter, `<impl>`). Show types and callables; show members when the type is expanded.
9. **The inspector is still verbose and clips.** Under "Sequence · measured", a nine-line caveat paragraph remains; move it to the ⓘ. The mini-sequence still clips at the right edge ("External boundary – lo…").

### D. Treemap and load

10. **Treemap controls.**
    - "Refresh treemap" and "All indexed files" are still buttons; Architecture dropped its equivalents.
    - With the fixed bands (#395), most of the area is in the top orange band.
    - Consider defaulting to a metric or aggregation that separates files (e.g. `mean`), and keep the ramp fixed.
11. **Startup load.**
    - Architecture takes 4.6 s and metrics 2.3 s.
    - `/status` (61 KB) is fetched four times in the first few seconds.
    - `/dependencies` (264 KB) loads at startup although nothing shows it until "Open in library".
    - **Fix:** load dependencies on demand, poll a small status digest, and cache architecture and metrics per revision.

### E. MCP (separate issue)

12. **The reported evidence level contradicts the results.** `workspace_describe` reports "syntax" as the evidence level for every language, and `find_symbols` warns "syntaxOnly", while rust-analyzer and scip-typescript data is fresh and `outgoing_calls` returns semantic, resolved bindings. Agents will under-trust the results. Report the evidence level per language from the imported evidence.
13. **`rules_check` pages are at the size limit.** A 25-instance page is 63 KB against the 64 KB response cap. Lower the default page size or trim the evidence per instance.

## Suggested slices (each its own issue)

**Already in the running app (2026-10-31).** These were the original slices 1–6 plus the 2026-10-29 slices 1–5:

- the brand restyle and Connect
- the simplified shell, Architecture and Treemap with metrics, and Files
- the Architecture payload
- tabs: no Libraries; the Sequence start list
- the Files and Inspector fixes

Confirm that `DESIGN.md` and `docs/design-refresh-contract.md` were amended along with the restyle.

Item numbers in brackets refer to the gap list above.

### Next

1. **Semantic bindings in Sequence** [1].
   - Participants come from resolved calls.
   - Standard-library calls are hidden as incidental, with a count and a toggle.
2. **Semantic bindings in Architecture** [2]. References and rules use resolved call and type bindings, with coverage reported per edge.
3. **Application layers only** [3, 6, 7].
   - Tooling sits beside the stack or is hidden.
   - The Sequence start list and the Classes rule consider application layers only.
4. **Architecture layout** [4, 5]. No collisions or truncation; one coverage sentence; counts behind the ⓘ.
5. **Files and Inspector** [8, 9]. Types and callables in the tree; caveats behind the ⓘ; the mini-sequence fits.
6. **Treemap and load** [10, 11]. Remove the orphan buttons; lazy-load dependencies; poll a status digest; cache per revision.
7. **MCP consistency** [12, 13]. Report the evidence level per language from the imported evidence; size `rules_check` pages under the cap.
8. **Remaining diagram views.** C4, and Pele parity for Class. No ERD.

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

- **Tooling layer:** draw `tooling` beside the stack (like Shared, but marked as dev tooling), or hide it by default like `fixture-inputs`? Either way, it leaves the Sequence start list and the Classes rule (gap items 3, 6, 7).

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
