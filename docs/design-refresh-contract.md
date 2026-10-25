# Workbench presentation contract

## Layout and assets

The approved layout and interaction reference is the user-provided UI reference. The owner-approved
SquashMerge brand supersedes only its palette, typography and logo treatment, as specified in
[DESIGN.md](../DESIGN.md#visual-tokens). Use ink/surface/raised backgrounds, decorative line and
meaningful control boundaries, and white/muted text. Primary actions use amber fill with ink text;
selection uses purple fill with ink text. Links are lavender and underlined; keyboard focus is a
2px lavender outline with a 3px offset. Status always combines a word and a shape with its color.
Heat uses line → gold → amber → orange, with a legend and ink text on warm fills, reversed for
Maintainability; this amendment adds no metric view or threshold policy.

Serve Manrope headings, Inter body/interface text and JetBrains Mono identifiers/source locally.
Retain the compact 13–14px body and existing pane layout, not the packet's larger layout or type
scale. Primary action buttons use a 44px minimum height and 8px radius.
The full logo lockup is Connect-only, with clear space. App headers use the provisional, unreviewed
symbol plus “Trellis by SquashMerge”; no favicon ships before a reviewed compact mark exists.
The Tools tab is retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). This does not add unshipped tabs or authorize fictional compiler, agent or terminal capabilities. Gated capabilities remain hidden until accepted.

The explorer stays beside the sequence canvas on desktop. Classes and Libraries have their
own tabs. Selected calls and controls populate the inspector. Explicit source actions open
a source dock. Small screens use exclusive explorer/inspector drawers and scroll diagrams
inside the canvas. Selecting a method or opening source closes obstructing drawers.

Retained form/control IDs, authentication and token opt-in remain.
All content is rendered as text. Font and script routes retain the existing Host/Origin/CSP
checks. Font licenses ship under `web/fonts/`.

## Controller interface

`window.TrellisShell` is presentation-only. It never fetches, indexes or calls a provider:

- `setConnected(bool)`, `updateWorkspace(status)`
- `showView("sequence" | "classes" | "libraries")`
- `showSource("workspace" | "library")`
- `selectStep(step, view, openSourceCallback)`
- `resetInspector()`, `reset()`

Application hooks are optional for legacy harnesses. Source callbacks retain the application's
session, revision and selected-method guards. Disconnect/revision/method changes clear stale
inspector callbacks. Pending/failed library reads remain visible in Libraries even if the dock
is closed. Successful reads reveal the selected cached source and highlighted range.

## Renderer interface

`render(container, view, readSource, expandedGroups = new Set(), options = {})`

`options.onSelect` inspects the original step. Without it, the legacy `readSource` callback
still runs. `options.showDetails` restores original verbose control labels. Group toggles
preserve keyboard focus, scroll position and selection. DTOs and measured evidence are immutable.

Control frames use clipped-corner `loop`, `alt` and `try` tabs, with bracketed guard/description
text beside the tab. Other structured containers use `block`. This is UML-style presentation,
not inferred UML semantics or an invented predicate. Clicking the tab or guard selects the
original step; Enter/Space do the same. Long full-detail labels wrap beside the tab, and the
canvas widens for deep input rather than letting guard text escape its frame.

Compact labels abbreviate known explanatory prose; they do not merge or remove guard,
alternate or exit nodes. Original labels/ranges/call IDs and nested evidence remain available
in titles, accessible labels, the inspector and Show all. Provenance uses labels/stroke patterns,
not color alone. No return values, activation durations, runtime identity or confidence are invented.

A collapsed flat chain previews only its first measured call. Its source action uses the original
full group range. Later receivers/return types may differ. Candidate library types remain unresolved
terminal boundaries; source hints are not type or dispatch resolution.
