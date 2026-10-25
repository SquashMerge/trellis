# Trellis interface direction

Approved by the user from a user-provided UI reference. The reference is a visual/interaction
model, not evidence of implemented compiler, agent, terminal or class-diagram capabilities.

## Planned unified workbench

Embedded terminal tabs are an accepted target, not implemented UI. Keep direct agents in real
PTY-backed tabs alongside source and diagrams; present ACP conversations/tools/approvals separately.
Optional Herdr attachment must use a verified supported transport and preserve external ownership.
See the [terminal contract](docs/terminal-workbench-contract.md). Do not add placeholder controls
that imply those capabilities already work.

## Product frame

- Compact explorer on the left; files expand to methods.
- Main tabbed workspace: Sequence, Classes, Libraries, Tools. No placeholder capability tabs.
- Classes uses the full canvas without the call inspector; class menus expose related classes and cached source.
- Selected-call inspector on the right. Move full source/provenance/control descriptions here.
- Explicit source reads open a collapsible source dock without losing the selected method.
- Source lines and class members expose revision-bound navigation menus. Use measured declarations
  and scoped type candidates, not global-name guesses. Source navigation is explicitly line-based.
- Workspace, revision, syntax/semantic state, indexing progress and failures remain discoverable.
- Desktop uses independently scrolling panes. Small screens expose explorer/inspector controls
  and contain the diagram scroll. Do not shrink a fixed desktop mockup into unreadable text.

## Visual tokens

The owner-approved SquashMerge brand replaces the reference's palette and typography, not its
layout or interactions. Use these dark-theme tokens:

- Surfaces: ink `#0B0C10` (page), surface `#14151D` (panels/cards), raised `#1D1E2A` (hover/quiet emphasis).
- Lines: line `#343644` for decorative dividers; control `#777A8E` for meaningful control boundaries.
- Text: white `#F7F7FB` primary and muted `#B8BAC8` secondary. Never use the control color for text.
- Warm colors: gold `#FFC20A`, amber `#FFAA10`, orange `#FF7A00`. Primary actions use amber fill
  with ink text. Heat uses line → gold → amber → orange, with ink text on warm fills and a legend;
  reverse the ramp for Maintainability. This does not add a metric view or decide its thresholds.
- Cool colors: lavender `#ABA5FF`, purple `#7770DC`, deep purple `#5D55C8`.
  Selection uses purple fill with ink text. Links are lavender and underlined.
  Keyboard focus uses a 2px lavender outline with a 3px offset.
- Status: success `#57D6A0`, error `#FF8E9A`, info `#85C7FF`; caution uses gold.
  Always pair status with a word and a shape, never color alone. Where diff markers exist,
  added uses success with `+`, removed uses error with `−`, and changed uses gold with `~`.

Use locally served Manrope for headings, Inter for body/interface text, and JetBrains Mono for
identifiers/source. Retain the compact body size of 13–14px and code labels of 12–13px where practical;
this brand amendment does not adopt the packet's larger layout or type scale. Primary action
buttons use a 44px minimum height and 8px radius; keep the surrounding panes compact.
Keep focus, disabled and loading states visible. Verify contrast on actual surfaces.

The full logo lockup appears only on Connect, with clear space. App headers use the provisional
symbol plus “Trellis by SquashMerge”. The symbol is unreviewed and awaits an official compact mark;
do not add a favicon until a reviewed compact mark exists. The design packet is a visual source,
not authorization to retire Tools, add unshipped tabs or expose gated capabilities.

## Diagram simplification, not semantic loss

Use short message names, compact control frames, quiet lifelines, and selection highlighting.
Analysis notes use short selectable rows; exact continuation guards stay visibly conditional with a rail.
Class views start with a focus and its indexed ancestors/descendants, collapsed members, and a chooser
for other related classes. Do not add unrelated peers merely because they share a base class.
Bundle returned class references by ordered pair; keep the original records in folded evidence.
Keep collapsed-chain entry arrows. A preview shows the first measured call, not a claim that
all calls share that receiver. Hide unused lanes and offer full detail through Show all.
Guard/alternative/exit evidence, original ranges and call IDs stay available. Summaries must
state when they represent multiple original elements. Never merge conditions by guessing
that identical labels mean identical semantics.

Resolved, syntactic, candidate and unavailable evidence remain distinguishable by labels/strokes,
not hue alone. No invented return values, activation durations, confidence scores, runtime traces,
Git branch names, provider activity or terminal sessions. Library calls remain terminal.

## Safety and continuity

No provider requests on connect, selection or source inspection. Existing explicit provider
confirmation/budgets and token opt-in behavior remain intact. Treat all content as text.
Keep source scopes separate and reset stale inspector callbacks across sessions/revisions.
Local assets stay within the existing CSP. Fonts include SIL OFL licenses under web/fonts/.
