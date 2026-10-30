# Navigate from source and class members

Navigation uses the current cached snapshot. It does not execute the inspected repository, fetch
library source, call a provider, or resolve a type by globally matching its name.

## Source pane

- Right-click a source line, or use **Navigate line N**.
- With the source pane focused, Up/Down or Home/End chooses a line. Shift+F10 opens its menu.
  Shift-arrow selection and normal copy remain available.
- Indexed callable declarations offer **Sequence** on hover or keyboard focus. Choose it to open
  that exact declaration in Diagrams › Sequence. Source text and normal copy remain unchanged.
- Source action metadata is read at the source pane's full index pin, in pages of 200 declarations.
  Large files offer **More source actions**; line menus remain available independently of those pages.
- Menus offer measured declarations on that line and the nearest enclosing method/class. Choose
  **Class diagram** for a supported type, **Inspect** for its declaration details, or
  **Read declaration source** to stay in source. Call graph is unavailable until its view ships.
- Browser Back returns to the same source position and selection in the current checkout session.
  Forward reopens the selected diagram. Logout, checkout changes and adopting another index pin
  invalidate old source-return actions rather than applying them to a different snapshot.

This is **line-based navigation**, not compiler go-to-definition for any word under the pointer.
Unresolved calls are not globally name-matched. Syntax candidates, same-class names, type hints and
sequence participant labels are not resolved callees. The enclosing measured method remains a
sequence choice when no eligible callee binding exists.

For resolved-callee navigation, the browser uses the sole published semantic producer whose profile
covers the source document's language at the source snapshot (owner ruling 2026-10-30, #483). With
none, it shows syntax choices only. With several, it also shows
`multiple semantic producers for <language>; showing syntax`. There is no producer picker. Menus label measured
declarations as **syntax** and eligible callees as **resolved**; selecting a producer alone does not
turn an unresolved, stale, external or dispatch-only binding into a callee action.

External/library source stays a separate read-only surface. Its catalog and source-hash identities
are not workspace syntax IDs, so it has no workspace Sequence or Class diagram action. Matching a
path or name does not change that boundary. Working-tree-only text likewise has no indexed actions.

Sequence URLs use `#sequence?sid=…`. A bare syntax ID resolves only after authentication and explicit
checkout selection, against that checkout's admitted snapshot. Generated links also carry the
checkout key and full index pin. A mismatched checkout or pin is a visible refusal, not an automatic
switch to another snapshot. Source return state is local to the current browser session; it is not
saved source text and cannot survive logout or a reload.

## Class members

Expand **Members**. Method names still open their sequences directly. Click a type hint, right-click
its member row, or use the row's visible menu button to inspect navigation choices. A method menu
can include both its own sequence and its declared parameter/return types. Primitive, unmatched,
unsupported or omitted types do not acquire fictional targets.

Java field names and types have separate syntax ranges. Their association is checked against a
bounded cached Java syntax tree and existing recorded type evidence, not neighboring text guesses.

## Continuity and limits

Opening pinned source reads declaration metadata for its inline actions, never another source file.
Selecting a line does not fetch navigation; its menu lookup remains explicit. Expanding members does
not fetch source. Menus are invalidated by scope/session/revision changes or dismissal. Opening a
new diagram clears stale source actions. Existing authentication, source guards and provider allowances
remain unchanged.

`POST /api/navigation` accepts either `{expectedRevision, path, line, semanticProducerId?, semanticProducerPolicy?}`
or `{expectedRevision, classId, memberName, startByte, endByte}`. The browser explicitly requests
`semanticProducerPolicy: "solePublished"`; an API caller may instead name one published producer.
With neither option, the API remains syntax-only. Member names and byte ranges must match
one stored member exactly. Results contain original measured symbols with action/reason/certainty,
plus warnings and partial-index/limit status. Type links remain syntax candidates, not resolved types.
The response is bounded to 512KiB. Evidence records and Java cached-source parsing have separate bounds.

See [implementation contract](member-navigation-contract.md).

Validation: [public historical summary](member-navigation-validation.json). Private-workspace browser
and deployment evidence is omitted; no validation rerun is claimed. No reindex is required.

Follow-up: [same-class calls and source/sequence action validation](call-navigation-validation.json).
