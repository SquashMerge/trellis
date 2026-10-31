# Architecture view

Architecture is a tab in Diagrams for reviewing dependencies and authoring reviewed
exceptions in the checkout. It shows authored layer bands and directed
**structural references · syntax evidence**, not compiler-resolved calls. A native module is one
indexed file with its full `(sourceSetId, language, path, sid)` identity. Paths are labels,
not inferred package names or identities; module chips (below) only group them for display.

## Layers and evidence

The fixed checkout file [`.trellis/rules.toml`](rules.md) supplies all layer names,
membership and order. Smaller integer orders appear above larger ones. Equal orders are
separate peers. Shared layers sit beside the ordered stack and remain separate. Empty
layers stay visible. Unassigned modules are explicitly **Unassigned / unknown** outside
the stack; Trellis does not guess membership.

Missing rules or a configuration without layers shows setup help for the existing
`trellis_propose_layers` MCP prompt. The browser does not call a model. Authoring
requires an existing valid rules file. Editing or removing authored exceptions does not
require layers or current matching instances; new layer-pair exceptions require
positively witnessed production deviations.
Invalid or ambiguous rules show the located `invalid_rules` diagnostic, including the
fixed path, line and column, without a partial diagram.

Layer edges count reference **sites**, not module pairs or rule-check instances.
Production references are solid. Test references are dashed and toggleable. Stored
roles determine this split: `cfg(test)` is test, while `cfg(feature="test-support")`
remains production under Decision 0011. The **Test-only edges** overlay does not change production reference totals,
policy or metric role selection. The separate **Include tests and fixtures in metrics** checkbox changes heat
without changing which reference edges are shown.

Downward references, including skipped orders, conform. Upward references deviate.
Cross-peer and shared-to-ordered references deviate by default; the independent
`layer_policy` overrides may allow them. Incoming references to shared conform.
Reviewed exceptions retain their underlying verdict and reason. Upward counts carry
the word **deviation** as well as error color; peer/shared deviations are not called
upward references. Same-layer sites remain in evidence totals rather than module arrows.
Unassigned endpoints and unresolved sites remain explicit unknown evidence.

## Module chips and counts (#424)

The view always draws the full stack: every ordered layer, with the shared layers beside
it (#449). Only authored `display = "hidden"` layers and unhinted test or fixture layers
wait in the hidden footer. Deviations are highlighted and allowed arrows (conforming or
excepted) are dimmed; no layer or arrow is dropped to focus on deviations. Each render
fits as many chip columns as the canvas width holds at that moment, and scrolls inside the
canvas when the headers need more; resizing the window or the inspector does not redraw on
its own (the next render refits).

An open band draws **one chip per module**, not per file. Bands start collapsed unless
their authored `display` is `"expanded"` (#391 ruling; see [rules](rules.md#layer-display-hint)).
A collapsed band draws one row of its module chips (a shared box: up to six stacked),
coloured by metric and hottest first, with a last chip counting the modules the row leaves
out; the band header opens it. Open bands charge the size-cap budget first, so summary rows
use only what remains and never fold a band the viewer opened. A module is a browser display
grouping computed from the existing response, not an identity or a new API field: the
first path component below the band's common directory, where a file and its same-named
directory form one module (`src/store.rs` and `src/store/**` are `store`), and a
directory's `mod.rs` or `__init__.py` is that module's root file. A one-file
module is that file's chip. Selecting a module chip opens it one level (its root file,
then nested modules) down to file chips; selecting a band header folds or opens it.
Chips show the module's maximum (minimum for Maintainability) displayed metric and its
measured file count; files without selected callables remain counted in a note.

Band headers show only the **non-zero** outgoing production reference-site counts as
compact chips: `↑` upward deviation and `⇄` peer or shared-outbound deviation in the
error colour, then `↓` downward, `→shared`, `allowed` (by `layer_policy`), `excepted`
and `unknown`. Arrows between bands carry the same counts (`↑52`, `↓8 excepted`), with
the direction and verdict in their accessible names. These are sums of the summary
edges, so they equal the `$layers` instances' `referenceCount` from
`trellis rules check --format json` on the same pin and rules.

Counts are lower bounds whenever their source layer has unresolved sites (#449): a
reference that maps to no module draws no arrow. A bounded count reads `↑≥102`, and a bounded
highlighted (deviating or unknown) arrow also shows the share of its source layer's sites
that map to a module (`↑≥102 · 9% mapped`). The accessible name says the same in words.
These figures come from the summary's measured `references.coverage`, never estimates. Unknown-verdict arrows are drawn in neutral grey.
A compact arrow key sits under the canvas with the whole snapshot's coverage
(`3,727 of 40,219 reference sites mapped to a module (9%) · 740 of 1,095 production rule
instances unknown`); the longer prose lives under the coverage notices.

The inspector opens on the counted review chips, **23 deviations · 156 unassigned ·
0 exceptions**, and every Architecture inspection keeps them at its head. They open the
review list (production deviations, which is what the first chip counts), the unassigned
files and the authored exceptions. Deviations are `rules.counts.deviates`, unassigned is
`rules.unassignedCount` and exceptions is `exceptionCount`. There is no refresh button: the
view re-reads when a new revision is selected or the tab is opened again. A detail list
that fails with `rules_conflict` offers **Reload Architecture**. On narrow screens the
overview does not open the inspector drawer.

The SPEC §9 size cap applies to chips: at most 40 per band or open module and 150 in
the whole diagram, counting every note. The rest of a list becomes one visible "N files
not drawn" note that opens the layer's complete file list in the inspector. Once the
budget is spent, further bands and modules stay folded and no more notes are added; the
status line reports the total not drawn. Test and fixture layers without a `display`
hint stay in the hidden footer until **Include tests and fixtures in metrics** is on.

Click a module or select it with Enter/Space to inspect its identity, layer, captured
physical file lines, callable count, every metric, and its incident references and rule
instances (read on demand, [below](#summary-and-detail-reads-448)). Select a
summary edge to inspect its exact contributing module pairs, production/test counts,
verdicts, exceptions and source-site samples. Omitted sample counts stay visible;
sampling never changes totals. Explicit source actions read cached indexed text at
the displayed revision. They never fall back to the live file.

## Summary and detail reads (#448)

`GET architecture` returns what the view draws plus exact counts, not per-site
detail. On this repository it went from about 35 MB to about 0.75 MB. Its fields are:

- `layers`, `modules` and summary `edges` (each edge keeps its contributing pairs and
  their bounded site samples);
- `references`: `referencesVersion`, `counts` (`scannedDocuments`, `mappedSites`,
  `unresolvedSites`), `limitations` and `externalPackages`, each descriptor with the
  display `layer` its `$layers` instances report (null when unassigned);
- `references.coverage` (#449): one row per source layer (null when unassigned) and
  stored site role, `{layer, role, sites, mappedSites, unresolvedSites}`. `sites` counts
  every scanned reference site once; a site with at least one mapped target is mapped,
  and one with a residual reason is unresolved, so a site can be both. The rows sum to
  `counts.mappedSites` and `counts.unresolvedSites`, and there are at most two per layer;
- `exceptionCount`: the authored `[[exceptions]]` entries (null without a rules file);
- `rules`: `rulesVersion`, `rulesFile`, `counts`, `testCounts`, `unassignedCount` and
  `limitations`.

`counts`, `testCounts` and `unassignedCount` (the length of `unassigned`) equal
`trellis rules check --format json` on the same pin and rules file. The reference
`counts` equal `trellis references`. The summary has no `modules`, `production`,
`tests` or `unresolved` reference arrays, and no `instances`, `testOnly`,
`unassigned` or `externalPackages` rule arrays.

`GET architecture/details` serves that detail one bounded page at a time. It requires
`kind`, the complete pin (`indexGeneration`, `indexRevision`) and the summary's
`rulesHash`. `offset` defaults to 0, and `limit` defaults to 100 (range 1–500). A
page also stops at 4 MiB of serialized items, but always holds at least one item.
The kinds and their exact-match filters are:

| `kind` | Item | Filters |
| --- | --- | --- |
| `instances` | `{role, instance}`: production `instances`, then `testOnly` | `role`, `rule`, `status`, `excepted`, `exceptionReason`, `sourceLayer`, `destinationLayer`, `layer` (source or destination), `path` (target or destination path) |
| `pairs` | `{role, sourceLayer, targetLayer, pair}`: production pairs, then test pairs | `role`, `path` (source or target path), `source` and `target` (an exact endpoint identity as JSON: a native target or an external package), `layer` (either endpoint), `unlayered=true` (same-layer or an unassigned endpoint) |
| `unresolved` | one unresolved site | `path` (source path) |

The response is `{indexGeneration, indexRevision, rulesFile, kind, offset, total,
nextOffset, items}`. `total` counts every matching item; `nextOffset` is null on the
last page. Items keep the evaluation's order and shapes, so the pages of one kind
concatenate to the CLI's arrays. Unknown or duplicate fields, filters that do not
apply to the kind, and an offset past `total` fail `invalid_request` (HTTP 400). A
stale pin fails like the summary (409 `revision_conflict` or `pin_expired`). A rules
file that changed since the summary fails 409 `rules_conflict` (or 422 `invalid_rules`
with the located diagnostic when the new file does not parse). A detail read
evaluates the same admitted snapshot as the summary, without metrics, and it never
indexes or writes. Existing Bearer, Host/Origin, deadline, cancellation and
response limits apply.

The browser reads details only when an inspector or list needs them:

- a module's pairs and rule instances;
- a layer's pairs and instances;
- an edge's instances;
- unresolved sites;
- same-layer and unassigned pairs;
- `$external` results;
- the review list;
- the witnessed deviations a glob selection chooses from.

An edge lists the `$layers` instances of its exact verdict and exception group;
`$external` results have their own list in the notices. Each list shows one page and a **Show more** button for the next one. Repository
text in details is inserted with `textContent`.

## Review and authoring

The review list pages rule instances from `architecture/details` with source/destination files, layers, production or
test role, underlying verdict and exception reason. Filters do not change the indexed
metric population. Evidence remains syntax-tier: captured source-owner identities and
spellings are not resolved destination-symbol bindings. Site samples and omitted counts
stay explicit. The Unassigned list includes files with no observed references.

An exception needs a nonblank reason. Select one pair or several witnessed pairs and apply
one reason in **one preview and one apply**. File-to-layer and glob controls select an
explicit finite list of current pairs; they do not persist wildcard exceptions or waive
future files. The portable format remains literal language/path endpoints under #365.
Unknown destinations and external packages cannot become portable pair exceptions.
Existing authored exceptions can be edited or removed even when no current pair matches.
An unassigned file can be assigned to an existing layer by adding its exact path.

Every operation shows the exact prospective `.trellis/rules.toml` diff before confirmation.
The editor preserves unrelated TOML formatting and comments. Confirm writes those exact
preview bytes atomically, without a Git commit, restart or index publication. The view
then checks the new rules against its indexed code pin. MCP remains read-only.

The browser-only checkout routes are `GET architecture/rules`,
`POST architecture/rules/preview` and `POST architecture/rules/apply`. Existing Bearer,
Host/Origin, request/response and rules limits apply. Writes are restricted to the fixed
checkout rules path. Each edit uses the loaded rules hash as well as the complete code pin.
If the file changed since load, HTTP 409 `rules_conflict` preserves the draft and requires
reload and a new preview; saving never retries or overwrites silently. Trellis serializes
its own writes, stages and syncs a temporary file, compares the current original immediately
before atomic rename, then syncs the parent. This is optimistic conflict detection, not
an OS transaction against arbitrary external editors in the final compare/rename interval.
Post-rename durability uncertainty is reported as a changed-file outcome, not “not written.”

## Metrics

The color choices are Layer, Cyclomatic, Cognitive, Max nesting, Maintainability,
Halstead volume and Fan-out. Layer mode uses neutral module chips and explicit
membership, not an invented numeric metric. Numeric aggregates are max (default), sum
and mean over raw [`metrics:v1`](metrics.md) callable scores. Heat explicitly starts with
`role=production`; the unchecked **Include tests and fixtures in metrics** checkbox requests `all` when checked.
Omitted API/CLI role remains `all`; `test` is available through those transports. The Architecture
payload echoes `role` (rather than `metricsScope`). Stored native declaration roles are authoritative
for Rust, Java, Python and JavaScript under Decision 0011, not browser path heuristics or runner
discovery. Inline Rust `cfg(test)` callables are test; feature-only `test-support` remains production.
Each module carries per-file `roleCounts: {production, test}` independent of the selected role;
its `declarationCount` and metrics describe the selected population. Fan-out is the measured callable metric,
not structural dependency degree. Null values stay unavailable, with reasons; they
never become zero. Test-only files have unavailable production heat, not a fabricated zero.
Callable-range LOC/SLOC can overlap and are not physical production file lines; no physical
production-LOC measurement is claimed.

Band headers show the maximum available displayed module value at the chosen
aggregate, not a mean of file means. Heat uses the shared
[fixed bands](metrics.md#workbench-heat-395) for max/mean. Sum is neutral and labelled relative
totals. **Relative**, off by default, uses modules in the admitted layers and the
unassigned inventory (drawn as chips even when collapsed), after measured filtering. Collapsed
layer/directory summaries include their member modules in that basis; expanding them does not
move the cuts. The hidden-layer footer can change the Relative basis. Maintainability reverses severity, not values.
Browser requests set `workbench=true`; stored tests, fixture roots, authored hidden layers and
`web/vendor/` do not contribute default heat. Counts report excluded callables and files.
Include tests and fixtures restores tests/fixtures/hidden-layer metrics, but not vendor metrics.
Module identities, physical lines, reference totals and Files access remain unchanged.

## Revision and rendering boundaries

One admitted snapshot supplies code evidence, assignments and metrics. The view retains
the full `{indexGeneration,indexRevision}` pin and the separate rules content hash:
indexed code does not pin live configuration. Evidence actions retain both. Changed
rules require refresh rather than explaining an old diagram using new rules. Checkout,
session and full-pin changes clear pending evidence and selection before rebinding. Metric-role
changes also clear old heat and inspection before fetching. Role participates in query, response
validation and currentness alongside the aggregate, root/session, full pin and request serial,
including async Pele completion and indexed-source callbacks.
Browsing does not index, execute repository code or contact providers.

Patched Pele 1.10.0 renders app-generated `block-beta` layer summaries. App-owned opaque
IDs map every interactive module and summary edge to its exact DTO identity. Repository
labels are inserted with `textContent`, not Mermaid syntax. A missing, duplicate or
changed renderer mapping clears the interactive canvas and shows an error; no guessed
or partial mapping is installed. Local assets and the existing CSP remain in force.
Natural-size diagrams scroll inside their canvas instead of shrinking text to fit.
Desktop panes scroll independently; narrow screens reuse Explorer/Inspector drawers.

Architecture remains syntax-tier evidence with `semanticComplete:false`. Coverage and
limitations are visible; missing references do not prove there is no dependency.
Module-level Flow is deferred to [#324](https://github.com/SquashMerge/trellis/issues/324).
The module inspector's **Open in Treemap** action opens the exact source-set/language/path
file at the same full pin. [Treemap](treemap.md) uses physical file lines (including
inline test lines) for area; its production heat and file visibility are independent.
