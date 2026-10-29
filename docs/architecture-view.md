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

An open band draws **one chip per module**, not per file. Bands start collapsed unless
their authored `display` is `"expanded"` (#391 ruling; see [rules](rules.md#layer-display-hint)). A module is a browser display
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

The SPEC §9 size cap applies to chips: at most 40 per band or open module and 150 in
the whole diagram, counting every note. The rest of a list becomes one visible "N files
not drawn" note that opens the layer's complete file list in the inspector. Once the
budget is spent, further bands and modules stay folded and no more notes are added; the
status line reports the total not drawn. Test and fixture layers without a `display`
hint stay in the hidden footer until **Include tests and fixtures in metrics** is on.

Click a module or select it with Enter/Space to inspect its identity, layer, captured
physical file lines, callable count, every metric and incident references. Select a
summary edge to inspect its exact contributing module pairs, production/test counts,
verdicts, exceptions and source-site samples. Omitted sample counts stay visible;
sampling never changes totals. Explicit source actions read cached indexed text at
the displayed revision. They never fall back to the live file.

## Review and authoring

The review list shows rule instances with source/destination files, layers, production or
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
totals. **Relative**, off by default, uses modules in the admitted layers and visible
unassigned inventory, after deviations focus and measured/involved filtering. Collapsed
layer/directory summaries include their member modules in that basis; expanding them does not
move the cuts. Quiet/hidden layer display filters can change the Relative basis. Maintainability reverses severity, not values.
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
