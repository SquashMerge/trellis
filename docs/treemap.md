# Treemap

Treemap is a read-only Diagrams tab. Patched local Pele renders generated
`treemap-beta`; it owns the squarified layout. No repository code executes and
no remote assets load. The displayed checkout and full index pin own every tile.

## Area and population

**Area: physical file lines · includes inline test lines.** Captured indexed text
is counted with Rust `str::lines().count()`, including blank and comment lines.
A final newline does not add a line; an empty file has zero area. Directory totals
sum included file lines once. This is not production-only LOC. Callable-range
LOC/SLOC can overlap and omit top-level code; they are never substituted for area.

The metrics response carries `physicalLines` and `fileRole` from the same indexed
document. `fileRole` is copied from its stored synthetic root-module role, never
recomputed from a filename in the browser. `roleCounts` still counts all measured
production and test callables before metric selection. The inspector distinguishes
mixed, production-only, test-only and no-callable populations. These describe
stored syntax roles, not test-runner discovery or full conditional compilation.
`cfg(test)` is test; feature-only `test-support` remains production (Decision 0011).

Defaults are Cognitive / max, production heat, and **Hide test files** enabled.
Hide test files removes a file with stored test role or a test-only callable
population (`production=0,test>0`). Mixed files remain visible. Zero/zero does not
imply production: the stored root role determines empty-file eligibility.
**Include tests and fixtures in metrics** independently requests all callables. It neither
reveals hidden files nor changes the area units. Showing test files leaves their
production heat unavailable, not zero.

## Heat and navigation

Color choices are Prod/test, Cyclomatic, Cognitive, Max nesting, Maintainability,
Halstead volume and Fan-out. Max/sum/mean are the existing raw callable folds.
The shared [fixed bands](metrics.md#workbench-heat-395) apply to max/mean; sum is neutral
and labelled relative totals. **Relative**, off by default, uses visible finite file quartiles;
ties remain together. Maintainability reverses severity because higher is better. The legend
always names the scale. Browser `workbench=true` excludes stored tests, fixture roots, authored
hidden layers and `web/vendor/` from default heat, with hidden counts. Include tests and fixtures
restores tests/fixtures/hidden-layer metrics, not vendor metrics. All file rows and physical area
remain; excluded files are still reachable through Files.
“—” means unavailable, not zero. Test files have dashed
outlines; full role/count evidence remains in the inspector.

Directories drill down and breadcrumbs go up. Path and language filters act on
indexed file identities. All positive-area files retain native tiles, even when
small labels do not fit. The equivalent file list also includes empty files;
it is the keyboard-accessible way to select zero-area and tiny files. No
unmeasured “smaller files” group stands in for source identities.

Click or Enter/Space selects a file. The lavender ring preserves its heat fill.
The inspector shows area basis, role counts, selected callable count and metrics.
**Read indexed file source** explicitly opens cached source at the same full pin;
there is no live-file fallback. Architecture's **Open in Treemap** passes the
complete file identity and pin, rather than finding a basename or guessing a path.

## Rendering and currentness

Pele `1.10.0-trellis.2` is unchanged; provenance is in `web/vendor/README.md`.
Opaque generated names map native `data-id` values to exact source-set/language/path
file identities or source-set/path directory identities. Native section/leaf
structure and the entire mapping are admitted before handlers are installed.
Missing, unexpected, duplicate or type-changed output fails visibly with no
partial interactive diagram. Repository text enters labels and SVG titles through
`textContent`, never Mermaid syntax, links, or styles. Labels fit only admitted
native geometry/font slots; unlabelled tiny tiles remain unlabelled.

The generated-source, SVG and DOM limits remain 512 KiB, 8 MiB and 12000 nodes.
No `font-size` appears in treemap classDefs. The layout fills the diagram box's
measured size, at least 640×360 px. When that leaves a positive-area file less
than 2500 px² on average, it grows in the box's aspect ratio, up to 4000 px per
side; Pele's nodeWidth/nodeHeight use one tenth of those pixel dimensions. A
resize relayouts the same admitted files once it settles, only when the layout
size changes, keeping the selection and without another metrics read. Large
diagrams scroll within the canvas rather than shrinking all text. Desktop panes
and mobile Explorer/Inspector drawers keep existing ownership.

The root section and its breadcrumb show the checkout directory's name from the
observed workspace root when one source set owns every indexed file; otherwise,
or without an observed root, they show the source set. The full label and SVG
title keep the exact source set.

Root, session, full index pin, role, aggregate and request serial fence responses,
async rendering and indexed-source actions. Scope/filter changes invalidate pending
renders and hidden selections. Checkout changes and refresh clear stale heat and
inspection before rebinding. Catching-up remains visible in the shared index header.
Browsing does not automatically index, write rules, or contact a model.

Whitespace-only Rust files currently expose an independent indexing defect
([#333](https://github.com/SquashMerge/trellis/issues/333)): publication fails before
Treemap receives a validated snapshot. Treemap does not invent unavailable rows or
fall back to live source for a failed publication. Its fix is separate from this view.
