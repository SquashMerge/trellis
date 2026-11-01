# Quieter diagram defaults

Simplify verbose sequence labels and large class views with repeated paths and member lists.
Simplify defaults, not evidence. Frontend-only; DO NOT edit native extraction, API, cache/schema,
providers or inspected repositories. Preserve snapshots/tokens/views/notes/budgets.

Sequence owner: web/sequence.js web/style.css tests/sequence-ui.test.cjs ONLY.
- Compact effect/return/definition/protocol boundary labels; no full RHS/return source by default.
  Keep every original step/call ID/range/order and conditional scope. No hiding calls/unsupported boundaries.
- Known generated continuation guard (exact label, no alternate) can use a quiet labelled guard rail
  rather than full alt box. It must remain visibly conditional; not an ordinary unconditional suffix.
  Other branches/loops/try keep structured frames. No merging/removing guards or alternatives.
- Non-call notes should be quiet rows/small indicators rather than full-width boxed paragraphs.
  Uncertainty remains visible in concise labels. Full original labels in aria/title/inspector and
  Show all steps restores full labels/frame presentation. Call lanes/collapsed entry previews unchanged.
- Synthetic regression example (not an observed run): call prepare_handler -> write -> nested-definition boundary ->
  continuation guard with PipelineStep/Pipeline calls and return. Assert all IDs/ordering, immutable DTO, guards,
  callback original fullstep, keyboard and full-detail restoration. Existing grouping tests stay valid.

Class owner: web/classes.js web/classes.css tests/classes-ui.test.cjs ONLY.
- Default starts with ONE focus class; no automatic24node spiderweb. Members show by default within
  the diagram size bounds (#559), with member counts and a reversible Members toggle. Qualification/path via disclosure/tooltip,
  not repeated on everycard bydefault. All original source/method actions retained.
- Right-click/visible menu Show related classes opens a bounded chooser (source-backed related names,
  directions/kinds, qualified identity todisambiguate); user chooses specific class(es) to add, not all24.
  Can use currentAPI unchanged: seed + expanded as selected related actualclass roots(max12), backend
  returns union neighborhoods; default render ONLY seed and expanded selected IDs. Every chosen class
  must remain visibly rooted/connected with actual originaledges. Reload fresh resets selection.
- Show all returned classes explicit reversible control for current boundedAPI result, not fullworkspace.
  Keep selected focus/expansions stable across detail toggle. Unmatched terminal hints available explicitly;
  do not infer types. API limits/stale/401/403/409/current session safeguards stay. Failed action recovery stays.
- One arrow per ordered class pair (bundle relationkinds, no reverse/direction invention), concise textual
  label (e.g. field / returns), not floating number soup. Every underlying relationship and original range
  stays accessible in collapsed evidence disclosure; callbacks unchanged. Bounded readable compact layout.
- Search results collapse after choosing class, reversible Change class action. Compact status line,
  one candidate caveat, diagnostic/evidence details folded. Add controls dynamically or coordinate IDs root.
- Existing rightclick/ShiftF10/touchmenus, ariaexpanded restoration and queued-scroll fix MUST survive.
  No source fetch except explicit action. Test focuseddefault, chooser add subset, member reveal, edge
  bundle preserving identities/directions, fullgraph/detail toggle, session/revision/failedrequests.

Root owns app.js shell.js index.html docs/deploy and browse-ui/shell-ui tests. Root will shorten surrounding
copy and add any requested toolbar IDs. Coordinate quickly. No global changes outside ownership.
No external assets/dependencies. Keep charcoal/flameorange/localfont theme, 13px+ primarylabels, contained
mobile scrolling. Public tests are synthetic; private screenshots/validation stay outside the public repository.
Run owned Node tests, report all changes/tests/freeze. Use synthetic methods/classes for public browser tests.
