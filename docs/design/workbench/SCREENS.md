# Screens

Each section lists: route in `trellis-workbench.html`, screenshot, source board in `reference/`, then what the screen does. "Needs" is the data the implementation has to get from the daemon. Where an existing contract covers it, the doc is named.

All app screens share a header:

- The symbol plus "Trellis by SquashMerge" (links home).
- The section nav: Workbench or Diagrams / Files / Design handoff.
- On the right, the daemon/index status as words plus a shape.

---

## Connect — `#Connect` · `screens/01-connect.png`, `01b-connect-choose-mode.png` · `Connect.dc.html`

Sign-in to the local daemon.

- **Layout:**
  - Full logo lockup on the left (about 280px of artwork, CSS-cropped, clear space kept).
  - The form card on the right. The layout wraps to one column when narrow.
- **Form:**
  - Daemon token (password input, mono).
  - "Load token file instead".
  - "Remember token on this browser" checkbox with its warning.
  - Primary **Connect** button.
  - Keep the existing token opt-in semantics (`docs/browser-token.md`).
- **After connecting:**
  - A "Connected to the daemon on <host>" status.
  - "How do you want to work?" with two cards:
    - **With Herdr**: shows "Session found" when detected.
    - **Diagrams only**.
  - A **Disconnect** button, which forgets a remembered token.
- **Needs:** auth handshake, host name, indexed checkout count, Herdr session detection (later).
- Ship the mode choice only once Herdr attach exists. Until then, go straight to the Diagrams-only shell.

## With Herdr — `#Main` · `screens/02-main.png` · `Main.dc.html`

Agent terminal and Trellis side by side, following the active agent.

- **Left: Herdr sidebar.**
  - Tree of **SSH machines** → repos (Local, `workstation · ssh`, `work · ssh`). These are Herdr's saved SSH machines; Trellis lists the same ones.
  - Each repo row shows a status dot, branch and behind count (e.g. `service-a main ↓12`).
  - Grouped **by repo only**, not by worktree.
  - Legend: working / needs input / idle (dot shapes differ).
  - The selected repo is a purple fill with ink text.
  - Below it, the file tree for the active checkout, with `M` markers for modified files.
- **Center: agent pane.**
  - Tab bar: Claude / Prime / Prime2 / Gotop / +. Each tab has a state dot, plus the cwd of the active tab.
  - **Only one pane is visible at a time.**
  - The footer states ownership: "Keyboard goes to Prime · Herdr owns this pane" and "trellis mcp · worktree issue-118".
- **Right: Trellis pane.**
  - Bound to the active tab's checkout/worktree: repo ⎇ branch, rev, Git ahead/behind and uncommitted count ("↑4 ↓0 · 3 uncommitted"), and the index state ("Catching up · 3 files").
  - Diagram tabs, with a **Follow active tab** checkbox.
  - A "Catching up" banner says the diagram stays on the measured revision until the watcher publishes. Modules the agent touched get a dashed gold outline.
  - Layer bands show their up-reference counts.
  - The bottom shows the selected module's pending edits, plus **Open diagram** and the primary **Design a change**.
- **Interactions:**
  - Switching tabs re-binds the Trellis pane: Prime → issue-118, Prime2 → issue-197.
  - The **Detach** button sits in the header.
- **Needs:**
  - Herdr session/tab metadata and the cwd → checkout mapping.
  - Per-checkout index state.
  - The changed-files-since-revision list.
- **Gated** on `docs/terminal-workbench-contract.md`: Herdr transport validated, ownership preserved.

## Without Herdr — `#Viewer` · `screens/03-viewer.png` · `Viewer.dc.html`

Diagrams only, no terminal.

- **Header:**
  - A checkout `<select>` (repo ⎇ branch · short rev · ↑ahead ↓behind), with the Git line beside the index state: "↑0 ↓0 · clean · as of last fetch".
  - Index state ("Fresh · rev 431").
  - **Attach Herdr…**, with a dashed **Later** badge: gated, not built in the first shell.
- **Left:**
  - The **Indexed checkouts** list, each with a status word: Fresh / Catching up · N files. The selected one is purple.
  - A note that any agent can still use these indexes through `trellis mcp`.
  - A **Files** tree with a filter input. Each folder and file has a metric swatch and value for the current color-by metric.
- **Center:**
  - Diagram tabs (Architecture / Treemap / Sequence / Class / C4) with a "Mermaid → Pele 1.6.0" tag. Class shows only for production code in a class-catalog language, and Libraries is not a tab (owner 2026-10-29, #450). There is no Tools tab (retired) and no ERD tab (dropped).
  - Architecture is shown: layer bands of module chips colored by metric, with each band's max and its refs down/up counts. Upward refs are boxed in the error color.
- **Right: inspector** for the selected module.
  - Every metric with its aggregation, with the active metric highlighted.
  - "Highest cognitive complexity" callables (name, file, cyclomatic, nesting).
  - Primary **Design a change**, with a dashed **Later** badge (gated on the handoff slice), plus a note on how the change is delivered without Herdr (open question).
- **Interactions:** the checkout select re-binds everything. The color-by and aggregation segmented controls recolor the chips and the file swatches.
- **Needs:**
  - The checkout list with index state.
  - The file tree (`docs/cwd-tree-contract.md`).
  - metrics:v1 per module, file and callable.
  - The layer map.

## Files — `#Files` · `screens/04-files.png`, `04b-files-working-tree.png` · `Files.dc.html`

Source view, shared by both modes.

- **Left:** file tree. Files expand to their symbols with kind and line (`struct`, `fn L71`), and the selected symbol is purple.
- **Center:**
  - Path, language and line count.
  - An **Indexed · rev 412 / Working tree · uncommitted vs HEAD** segmented toggle, with a basis line: "Cached source from revision 412 · navigation and diagrams use this basis".
  - Line-numbered source, with the selected method's range highlighted (watch.rs 71–79).
  - **Working tree** shows the file's `git diff HEAD` hunks (hunk headers, − and + lines) with the note "Not indexed. Navigation and diagrams use rev 412."
- **Right:**
  - The selected method (`WatcherHealth::witness`, its range and return type).
  - A measured workstation-sequence with **Open full**.
  - "References to WatcherWitness · 4" as linked locations with a snippet, plus the honesty note "Syntax matches in the indexed revision. Not compiler-resolved."
  - With Herdr only: an agent card with the status and a "Send selection to agent" input.
- **Needs:**
  - Revision-bound cached source and the working-tree read.
  - Symbol outline and references (`docs/member-navigation-contract.md`, `docs/navigation-reference.md`).
  - The sequence DTO.

## Design handoff — `#Handoff` · `screens/05-handoff.png`, `05b-handoff-verify.png` · `Handoff.dc.html`

Design-first change request. **Planned: SPEC §11, not scheduled.** A dashed "Planned" badge sits in the header.

- **Steps:** 1 Edit diagram → 2 Review changes → 3 Send to agent → 4 Verify.
- **Canvas:**
  - The class diagram with an **Overlay / Before / After** toggle.
  - Edit tools: Move / Add / Connect / Rename / Remove.
  - **The diff is drawn in the diagram itself.** There is no class-text panel.
    - Added: green + badge, solid green outline.
    - Removed: red − badge, dashed red outline, struck-through text.
    - Changed: gold ~ badge, with + and − member lines.
- **Right:**
  - "Changes in this diagram": a counted list, each change anchored to a source location.
  - "The agent receives": a typed change request built from the *structural* comparison of the generated and edited diagrams (SPEC §11, Decision 0014), each change anchored to its indexed source location. Not a Mermaid text diff. Example: "classDiagram · 5 structural changes · 4 source anchors".
  - "Send to a tab": radio list of Herdr tabs, each with its state. Recommended: a new tab in a new worktree.
  - Primary **Send change request to agent**, with the note "Trellis never edits the repository."
- **Verify:** after the agent finishes and the code is re-indexed, each change is reported as applied / partially applied / differs. Here, 1 differs: `http.rs:720`.
- **Needs:** everything in SPEC §11, including a structural Mermaid diff, stable ID anchoring, the agent path and re-index comparison. Don't build this until it's scheduled.

---

## Diagram views

All diagram views share:

- A card header: repo ⎇ branch, rev, and "Mermaid <type> → Pele 1.6.0".
- Tabs across the top.
- An inspector on the right.
- An honesty footnote stating the basis (measured vs. authored vs. approximate).

### Architecture — `#Architecture` · `screens/06-architecture.png` · `Architecture.dc.html`

- **Layout:**
  - Layer bands ordered by each layer's `order` number from `.trellis/rules.toml` (#129), shown in the label: `1 · UI`, `2 · HTTP & daemon` and `2 · MCP & ACP` (**peers**, side by side in one row), `3 · Query services`, `4 · Indexing`, `5 · Storage`.
  - The **Shared** layer (model, testing, utilities) is a dashed vertical box to the right of the stack, spanning its height: "Shared · any layer may use". It carries its own deviation count, because shared code calling into an ordered layer is a deviation by default.
  - Each band shows module chips with their metric value and the band's max.
  - Edges: downward references on the left (neutral, allowed); upward ones on the right (error color, deviation); a cross-peer edge between the two order-2 bands and a shared → ordered edge from the Shared box (both deviations, ←); test-only edges dashed, with a **Test-only edges** toggle. A legend under the canvas names all four.
- **Color by:**
  - Options: Layer / Cyclomatic / Cognitive / Max nesting / Maintainability (inverted) / Halstead volume / Fan-out.
  - Aggregation: **max / sum / mean**.
  - Legend: low, then the 4-step ramp, then high.
  - The "Prod/test" mode shows the production references solid and the test-only ones dashed (from #163). They're toggleable.
- **Inspector:**
  - Selected module: layer, lines, callables.
  - Every metric with its aggregation.
  - A link to "Open its files in the treemap".
- **Needs:** the layer map, module → module reference counts split by production vs. test-only (cfg(test) compiled out), and metrics:v1 per module.

### Treemap — `#Treemap` · `screens/07-treemap.png` · `Treemap.dc.html`

- **Layout:**
  - Area = lines; nested folders → files.
  - Each folder header shows its total lines.
  - Each tile shows its name and metric value.
  - Small files are grouped as "N smaller…".
- **Color by:**
  - Options: Prod/test, Cyclomatic, Cognitive, Max nesting, Maintainability (inverted), Halstead volume, Fan-out. Aggregation max / sum / mean.
  - Test files get a dashed outline. **Hide test files** removes them.
- **Selection:** a lavender ring, which doesn't hide the fill color.
- **Inspector:** file, lines, callables, production/test, and every metric. "—" means no callables were measured.
- **Needs:** per-file lines, the production/test classification, and metrics:v1 per file.

### Sequence — `#Sequence` · `screens/08-sequence.png` · `Sequence.dc.html`

- Measured sequence for `Store::navigation_at` (`src/store.rs:1732`).
- Follow the sequence fidelity rules in SPEC §9:
  - Fresh exact internal targets from the seed language's sole published semantic producer (#483) have declaration lanes. Measured call steps draw call arrows; source boundaries keep their context as notes, without an unconditional arrow. Select a row to inspect its original call site. Callable targets offer **Open callee sequence**; other declarations offer **Read declaration source**, at the same revision.
  - Unbound, unresolved, ambiguous, stale, external and dispatch-boundary calls are labelled from engine evidence, with no fabricated declaration targets. Zero or multiple producers for the seed's language keep the measured syntax-only view.
  - Collapsed chains preview only the first measured call and its proven target. Hidden or folded later calls do not introduce lanes. Full and Files mini sequences use the same targets and history owner.
  - No invented returns or activations.
  - Every guard and alternate is preserved.
- **Entry (#450):** a method from the tree, the source margin or the inspector. With no method selected, the tab lists up to five named most-complex methods of the current metrics projection to start from.
- **Needs:** the existing SequenceView DTO (`docs/sequence-slice-contract.md`).

### Class — `#Class` · `screens/09-class.png` · `Class.dc.html`

- A one-hop view seeded on `WatcherWitness`, with expand-neighbors on nodes.
- Respect the size cap; collapse the frontier into package nodes.
- **Shown only (#450)** when the checkout has production code in a language `GET /api/classes` reports in `supportedLanguages`; hidden otherwise.
- **Needs:** the class diagram DTO (`docs/class-diagram-contract.md`).

### C4 — `#C4` · `screens/10-c4.png` · `C4.dc.html`

- Containers from `docs/architecture/agent-integration-containers.md`.
- Labelled **Authored diagram**: a proposal, not checked against indexed code.
- Each node has a status (Shipped / Proposed / External).
- The inspector lists relationships.
- **Mockup bug:** a few edge labels collide or clip ("Launches owned child · real PTY" over the boundary title, "Authentic…"). Pele layout replaces this.

### Libraries — in context, not a tab (#450) · `screens/11-libraries.png` · `Libraries.dc.html`

- **Owner decision 2026-10-29:** the Libraries board is no longer a tab (reversing the 2026-10-24 ruling). The board shows the catalog's content, which now opens in the source dock's **Library** tab.
- **Entry:** **Open in library** on an Architecture reference pair whose target is an external package, and in the Sequence step inspector for an external call whose SCIP symbol names a package. It appears only when the local catalog indexes that package; several versions offer a choice.
- **Dock:** the package with version, source and index state as words, folded library diagnostics, indexed definitions with a filter and paging, and the badges **Syntax candidates · Terminal library boundary**: declarations don't prove resolved types, callees or dispatch. Selecting a definition reads its candidate source explicitly. No downloads or project commands.
- **Needs:** `GET /api/dependencies`, `/dependencies/symbols`, `/dependencies/source` (unchanged) and `callEvidence.externalPackage` on sequence steps.

### ERD (removed)

The measured-from-`CREATE TABLE` ERD board is dropped. A live ERD from configured databases (SPEC §10) is future work.
