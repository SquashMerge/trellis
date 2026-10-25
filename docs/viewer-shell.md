# Viewer shell

The Viewer has Diagrams and Files sections within one checkout/session frame. Diagrams keeps
Sequence, Architecture, Classes and Libraries and its collapsible source dock. Architecture shows
authored layer bands over pinned structural-reference evidence; see the
[Architecture contract](architecture-view.md) for counts, stored roles, metrics and limits. Files is a primary cached-source
surface with a pinned indexed-file catalog, all-symbol outline and declaration evidence, as
specified in the [Files contract](files-view.md). Section switching alone never chooses a file,
reads source or starts indexing. Explicit indexed source reads remain cached and revision-bound.
Files may separately show the existing net HEAD-to-working-tree Git diff; it is uncommitted and
not indexed, never a replacement for cached source or its navigation basis.
There are no remote-machine groups, future diagram tabs, agent/terminal controls or gated actions.
The existing Git summary remains independent of index freshness; upstream counts are as of last fetch.

## Checkout observation

`GET /api/checkouts` remains observational. Existing `rootKey`, `workspaceRoot`, `state`,
`active` and `git` retain their meanings. Each row additionally carries:

- `revision`: the complete `{indexGeneration,indexRevision}` pair, or null without an admitted head.
- `catchingUp`: an active runtime's admitted observation, or null when no watcher owns the checkout.
- `indexError`: null on success, otherwise a typed observation failure such as `index_not_ready`,
  `storage_busy`, `corrupt` or `unavailable`. Selection availability is not freshness.

Runtime references are captured under the registry lock and read outside it. Listing never
activates a checkout or starts indexing. Cold published indexes are observed through the existing
read-only identity/status path; their pin is shown as **Indexed · idle**, not Fresh. An active
`catchingUp:false` means **Fresh at read admission**, not proof that disk cannot change or that
watcher delivery is complete. True is **Catching up**. Failures and missing heads stay explicit.
The watcher does not publish a pending-file count, so no count is guessed from dirty generations,
SCIP drift, modified Git paths or private watcher batches.

Both checkout entry buttons and the native header selector use the same explicit selection path.
Connecting never picks a checkout. Checkout, session or full pin changes clear metrics, tree,
source callbacks and open views before rebinding; equal revision numbers in different generations
are not equal pins. Existing busy/replacement and authentication semantics remain intact.

## Metric tree and inspector

Use [`metrics:v1`](metrics.md), explicitly scoped to **production callables** by default. The
unchecked **Include tests in metrics** checkbox switches the selection to `all`; unchecking it
requests `production`. API/CLI omission still defaults to `all`, and `test` is available there.
Stored native declaration roles are authoritative for Rust, Java, Python and JavaScript under
Decision 0011; the browser does not classify filenames. Default color is cognitive complexity
and default aggregate is max. Max, sum and mean are server projections over declaration
scores, never client folds of file aggregates. One revision projection supplies file and directory
swatches; one selected-scope request supplies the inspector and `topCognitive=3`.

The Diagrams Explorer remains live filesystem metadata. Only `indexedPath` joins file evidence.
Directory paths must map inside the verified indexed workspace; outside paths, symlinks, nonindexed
files and ambiguous source-set/language matches have no metric evidence. Expanding a path for
metrics never reads source, starts indexing or calls a provider. Compacted directory chains inspect
their terminal displayed directory. Filtering still searches loaded entries only. Files instead
uses the pinned indexed-file catalog: its cached source must remain reachable when a live file
has been deleted. That catalog is not a claim about the current filesystem or Git changed set.

The inspector shows every metric leaf at the named aggregate. Null stays `—`; explicit unavailable
reasons, including fan-in, stay visible. Highest-cognitive rows use raw callable scores and native
nullable names, kinds and byte ranges. Anonymous functions are not assigned invented names.
These rows are informational, not guessed native-SID-to-source links. Labels and highest-cognitive
rankings follow the selected role. Test-only files have unavailable production heat, not zero.
Callable-range LOC/SLOC may overlap and are not production physical file lines. Per-file
`roleCounts` describe all measured production/test callables at the pin, not physical line coverage.

Heat cut values are quartiles of finite metric values on the **currently displayed file/directory
rows**, after loaded-entry filtering, expansion and pagination. Sort values ascending; for n values
choose cuts at `min(n-1,floor(n*p))` for p=0.25,0.5,0.75. The band is the number of cuts strictly
less than the value, so ties remain together and repeated cuts may leave empty bands. The legend
shows the actual cuts (`≤a`, `>a–b`, `>b–c`, `>c`); an empty sample has no invented zero cuts.
Maintainability reverses color severity only, retaining the original values and higher-is-better label.
Color selection does not fetch; aggregation refreshes the pinned projection. Role changes clear
old heat and inspection before fetching; response-currentness checks include role alongside
aggregate, checkout/session, full pin and request serial. A late same-pin response from another
role cannot repaint the current choice. Failures clear stale
metric values and show an actionable error. The existing 15-second browser request timeout remains;
a timeout is not evidence that the server finished or that values are zero.

Within Diagrams, only one inspector selection is displayed: a source step or file/directory
metrics. Classes keeps its full canvas unless a file/directory metric inspector was explicitly
selected. Clearing it restores that layout. Files replaces that inspector with the explicitly
selected declaration's evidence; source locations, syntax matches and mini-sequences remain on
the shown full pin. Full-pin, checkout and session changes clear derived evidence before rebinding.
Desktop panes scroll independently; at 900px and below, Explorer and Inspector use the existing
mutually exclusive drawers. Files source is primary, not an Escape-dismissable secondary dock.
Source navigation retains its shared keyboard/menu behavior; Git diff rows never become indexed
navigation targets. All repository text is written with `textContent` under the existing CSP and
local assets. Existing authentication, Host/Origin checks and token behavior remain unchanged.
