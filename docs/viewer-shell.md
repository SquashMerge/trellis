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

## Displayed revision and observed head

A successful retained-pin read keeps its exact displayed revision and evidence. After a
pinned browser read (including cached source), the app separately observes current status.
If the same generation has a newer head, the shared frame shows the viewed revision,
“Older than head”, the observed head and an explicit **Reload at head** action. At-head
results show no older indication. A different generation is labelled rebuilt, not ordered
by revision number. Catching up and Git state remain separate observations.

Head observation never adopts a pin, writes state or starts indexing. It is advisory,
not an atomic head-at-result-admission guarantee. Failed comparison keeps valid displayed
evidence; a later pinned read can try again. Session, checkout and full-pin guards prevent
late observations from changing another view. There is no background comparison timer.

Reload at head re-reads status through the existing explicit refresh path. A changed pin
clears revision-bound selections before rebinding; choose the file or method again at the
new head. Old symbol IDs and ranges are never carried across pins. Retired saved views
and notes are not restored by this presentation behavior.

## Metric tree and inspector

Use [`metrics:v1`](metrics.md), explicitly scoped to **production callables** by default. The
unchecked **Include tests and fixtures in metrics** checkbox switches the selection to `all`; unchecking it
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
their terminal displayed directory. Files the server reports as an unsupported source type
collapse into one quiet marker per folder that keeps their count and names; other unindexed
reasons keep their own rows. Filtering still searches loaded entries only. Files instead
uses the pinned indexed-file catalog: its cached source must remain reachable when a live file
has been deleted. That catalog is not a claim about the current filesystem or Git changed set.

The inspector shows every metric leaf at the named aggregate. Null stays `—`; explicit unavailable
reasons, including fan-in, stay visible. Highest-cognitive rows use raw callable scores and native
nullable names, kinds and byte ranges. Anonymous functions are not assigned invented names.
These rows are informational, not guessed native-SID-to-source links. Labels and highest-cognitive
rankings follow the selected role. Test-only files have unavailable production heat, not zero.
Callable-range LOC/SLOC may overlap and are not production physical file lines. Per-file
`roleCounts` describe all measured production/test callables at the pin, not physical line coverage.

Heat uses the shared [fixed per-callable bands](metrics.md#workbench-heat-395) by default,
including in the explorer. Explorer rows show a swatch and number; whenever they do, an explorer
legend names their metric, aggregate and population and states the bands, in every Diagrams view.
Max and mean use the same bands; sum is neutral
and labelled relative totals. **Relative**, off by default, explicitly uses quartiles of the
currently displayed finite file/directory rows after loaded-entry filtering, expansion and
pagination. The legend always names the scale. Maintainability reverses severity, not values.
Browser requests set `workbench=true`: tests, fixture roots, authored hidden layers and the
vendored `web/vendor/` directory do not contribute default heat. Hidden counts describe excluded
callables/files, not physical lines. Include tests and fixtures restores that population except
vendor files. Files still exposes excluded indexed source.
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

## Treemap

The [Treemap tab](treemap.md) shows nested indexed folders and files with physical-line
area, explicitly labelled “includes inline test lines”. Cognitive/max production heat
is the default. Hide test files is independent of Include tests and fixtures in metrics. The
revision-bound inspector and explicit indexed-source action share the desktop panel
and mobile drawer with the other views. Tiny and empty files remain in an equivalent
keyboard file list. Architecture can open an exact file at the same full pin.
