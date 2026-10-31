# Files section

This contract implements the scheduled [#311 design note](https://github.com/SquashMerge/trellis/issues/311#issuecomment-6396049897).
It narrowly extends the [Viewer shell](viewer-shell.md); it does not change historical semantic
contracts, extraction, schemas, MCP or providers. The updated design board is visual intent,
not authorization for Herdr controls, future tabs or gated actions.

## One checkout and one indexed basis

Files is a real section of the existing shell. Diagrams retains Sequence, Classes, Libraries
and its source dock. Files reuses the same checkout/session/full-pin owner and the existing
cached workspace-source cache and navigation. It does not create a second source cache or
independent checkout selection. Entering Files selects no file; arrival after Connect may select the
only or last used checkout ([Viewer shell](viewer-shell.md#checkout-observation)).
Only explicit file/source/location actions read cached source. Section switches, folder expansion
and filtering never start indexing or call providers.

The full indexed pin is `{indexGeneration,indexRevision}`, not its revision number alone.
Indexed text, outline, selected declaration, source navigation, match locations and mini-sequences
all retain that shown pin. Checkout, session and full-pin changes clear derived evidence and
callbacks before rebinding. Superseded requests cannot publish success or error over a newer
selection. A stale/mismatched indexed reply is an explicit failure, not permission to use latest
source or partially update the inspector. Existing authentication and stale-pin behavior remain.

## Cached file catalog and all-symbol outline

Files uses the pinned indexed `files` catalog for its tree. A non-empty filter asks the same
catalog for matching paths (`q`, at most 256 bytes) instead of filtering loaded pages. The Diagrams Explorer remains a
live filesystem metadata browser; it is not the Files catalog. This distinction keeps captured
files reachable after live deletion and prevents a directory listing from silently changing the
indexed basis. A cached path need not exist on disk now. Conversely, a live-only or Git-only path
has no fabricated indexed outline, source navigation or diagram basis.

Explicit indexed-file selection opens its cached source and paged measured outline. It resets
the new file to Indexed and clears the previous declaration's inspector, matches, mini-sequence
and Git diff. Selecting a noncallable remains useful; it must not be hidden because `/methods`
is callable-only. `/methods` keeps its existing contract unchanged.

`GET /api/checkouts/{rootKey}/symbols?path=...&indexGeneration=...&indexRevision=...&offset=0&limit=200`
returns `{revision,path,items,nextOffset,truncated,warnings}`. Each item is
`{symbol,nativeKind,header}`: `symbol` is the existing full measured `Symbol`, and `header` is
the captured native `Header`. Source range and parent identity remain those of the original
symbol. Nullable captured metadata, including `header.resultType`, stays nullable. Do not invent
a return type, name or finer declaration kind from source regexes or optional SCIP labels. The
default and maximum page size are 200. Order is source order, then original symbol ID; paging
must preserve that order. `nextOffset` identifies the next page when present.

The native declaration `SyntaxId` is already the projected graph `Symbol.id`. Callable sequence
seeds use that exact ID, never a name join or a guessed ID translation. Retaining `nativeKind`
matters because projection can coarsen native kinds, such as a constructor to a graph method.

Native byte ranges are zero-based, half-open UTF-8 ranges. Graph source ranges have 1-based lines
and 1-based byte columns; only LF ends a line. DOM selection must convert byte boundaries rather
than slice JavaScript UTF-16 offsets. An end at column 1 on the next line does not highlight that
next line. Display and navigation preserve the original path, range and full pin.

## Syntax matches for the selected declaration

`POST /api/checkouts/{rootKey}/syntax-matches` accepts the closed request
`{expectedRevision,symbolId,limit?,continuation?}`. `expectedRevision` is the complete pin and
`symbolId` identifies the exact selected declaration. The service derives its captured spelling
and scans only cached files in that declaration's source set and language. It recognizes
identifier/type/property-name syntax leaves with the same spelling. It excludes comments,
strings and the selected declaration's own name. It does not scan live bytes, other checkouts
or dependency roots.

The panel heading is **References to X · N** (#451, as in the design), where X is the matched
spelling and N the rows shown. N reads **≥N** while more pages remain or coverage is partial,
and **unavailable** without a spelling. Directly under it, the note **Syntax matches in the
indexed revision · same spelling, not compiler-resolved** explains that
homonyms, shadowing, unrelated properties and other declarations may share the spelling. The
query concerns the selected declaration, not an illustrative return type in a design mockup.
Results are original pinned source locations with cached snippets, not bound symbol targets or
proven references. Optional SCIP is display-only and cannot justify a resolved-reference label.
A nameless or otherwise unmatchable declaration needs an honest unavailable state, not a claim
of zero references.

This is a request-time syntax scan, not a new stored reference index. Its query-work bounds are:

| Bound | Limit |
| --- | --- |
| Results per page | Default 50, maximum 200 |
| Complete encoded response | 512 KiB |
| Cached source per parsed file | 2 MiB |
| Parse and walk per file | 250 ms |
| Total source input per request | 16 MiB |
| Total parse and walk per request | 1 second |

A parser progress callback and iterative walk enforce work bounds; checking elapsed time only
after parsing is not sufficient. These are authorized query-work limits, not new stored-row
re-attestation or unrelated per-field read caps under [T00](local-topology.md#t00--threat-model).

The response is `{revision,symbolId,evidenceKind,scope,items,continuation,partial,truncated,warnings}`.
`evidenceKind` is `"syntaxMatches"`. Scope is `{sourceSetId,language,spelling}`; `spelling` may be
null when name matching is unavailable. Each item is `{path,sourceHash,range,snippet}` with an
exact graph `SourceRange`. `snippet` is a cached-source string; clipped excerpts visibly use
ellipses while the exact match range remains intact. A snippet is not the navigation range.

Continuation is null or the object `{revision,symbolId,path,offset,partial}`. Clients echo the
object unchanged as the next request's `continuation`. It is tied to the full pin and selected
symbol. Results follow manifest-path then source-byte-offset order. The cursor never advances
past an unreturned match, including when the complete-response bound ends a page. A later page
cannot silently rebind to another pin or declaration. `truncated` means continuation is present;
`partial` records incomplete coverage and carries skipped coverage through subsequent pages.

Budget exhaustion, skipped files and unsupported states explicitly report incomplete coverage.
A page's row count is not the total reference count. Distinguish a complete empty result from
unavailable or partial results; do not label incomplete coverage as zero references. Malformed
stored evidence remains a typed whole-read failure, never a partial successful scan.

A match click is explicit intent to open its exact pinned cached location. It returns the center
to Indexed and clears the previous declaration inspector, so that inspector cannot claim to
describe the new occurrence. Choosing an actual outline declaration can then bind new evidence.

## Source navigation and mini-sequences

Indexed source retains the existing shared line-navigation menu and keyboard behavior.
The selected declaration's range uses the raised surface with a lavender left bar, not a purple
fill (#451). Source-margin Sequence actions are labelled by declaration name; the opaque symbol ID
stays out of accessible labels and tooltips. It is
line-based navigation, not compiler-resolved word-at-cursor navigation. Keep the source area
focusable, Navigate and Shift-F10 actions, Up/Down line movement, native text selection/copy and
menu dismissal/focus return. Full-pin/session/checkout checks apply to delayed menu actions.
All source actions read cached text explicitly; no action substitutes current disk content.

Only callable declarations have mini-sequences. Use the actual pinned `SequenceView` with the
existing faithful renderer, scaled down to the inspector's width (#451) in a contained vertical
scroll area; Open full shows it at full size. Keep original IDs/ranges,
guards, alternatives, exits, unresolved or terminal boundaries, hidden/truncated counts and
warnings. Do not replace it with guessed arrows, a source snippet parser or a falsely complete
first-call sample. Noncallables have an honest no-sequence state, not an invented empty diagram.

Open full promotes the exact callable seed and pin to Diagrams/Sequence. It selects the Sequence
tab as it enters Diagrams, so the last-used diagram tab neither shows nor refreshes (#451). It uses the same admitted
projection, or explicitly requests the intended projection under that seed and pin. It never
silently asks for latest. Mini-sequence source callbacks retain the same pinned cached-source
rules as the full view.

## Working tree is separate live Git evidence

Files reuses the existing read-only [Git API](git-status.md) unchanged:

- `GET /api/checkouts/{rootKey}/git`
- `GET /api/checkouts/{rootKey}/git/diff?path=...`

Working tree is the **net HEAD-to-working-tree difference, including staged and unstaged changes**.
It is not an index-to-worktree comparison and does not assert that HEAD equals the indexed pin.
Label it **Uncommitted · not indexed**, and state that navigation and diagrams retain the shown
indexed pin. Indexed text is never replaced by live bytes or reconstructed diff text.

Render the exact structured Git DTO: file path, previous path, status and binary state; hunk
`oldStart`, `oldLines`, `newStart`, `newLines`, `header`; and line `kind`/`text`. Preserve exact
text and terminators. Show hunk headers and visible `+`/`−` markers, not color alone. Derive old
and new gutters from the hunk counters: context increments both, deletion only old, addition only
new. Diff line numbers are not indexed source coordinates and never become navigation selectors.

No Git hides the Working tree toggle and shows the measured unavailable reason. Unchanged files
show an honest no-uncommitted-changes state. Binary files have no textual hunks; renames show the
previous path; deletions retain removed hunks. Unavailable, conflict, `git_changed`, response-limit
and status/diff race states remain explicit and actionable, not silently clean. Git requests are
fresh observations, not one atomic snapshot across requests. Existing Git bounds and tagged
failure meanings remain unchanged. Git does not need a ready semantic index or activate one.

Changed-only paths may be listed separately, but do not acquire fabricated indexed evidence.
Changing basis changes only the source/diff surface: declaration evidence and diagrams keep their
indexed basis. Leaving Working tree or changing file invalidates its pending diff; later replies
must still match checkout, path and current selection. Git never changes the indexed pin.

## Layout and safety

Desktop panes scroll independently; source remains readable, and the mini-sequence is a preview
scaled to the inspector's width, with Open full for reading. At 900px and below use the existing mutually exclusive Explorer/Inspector
drawers. Hidden sections and inspectors leave the focus order. Source in Files is primary and
must not disappear as though Escape closed a secondary Diagrams dock. Retain shared menu/drawer
Escape behavior and restore focus to a visible current control.

All repository paths, names, snippets, source, diff content and errors use `textContent`. Keep
existing local assets, authentication, Host/Origin enforcement, CSP and token behavior. No
provider calls, implicit indexing, live-source fallback or new execution capability is added.

## Acceptance evidence

Verify noncallable outlines and original IDs/parents/headers; pagination and UTF-8 ranges;
cached-vs-live divergence and cached deleted-file reachability; source-navigation keyboard
behavior; syntax evidence labels, snippet clipping, paging, continuation and stale fences;
partial/unsupported/budget states versus typed corrupt failures; faithful mini/Open full identity;
and exact Git DTO hunks, no-Git and status/diff-race states. Include real-daemon browser evidence,
desktop/mobile Indexed and Working tree captures, and independent source review. Canonical
verification includes formatting, both Clippy configurations and `./tools/verify`; this contract
is not a claim that those checks have run.
