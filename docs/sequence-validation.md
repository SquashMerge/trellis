# File/method sequence validation

## Outcome

The inspector now opens indexed files as a tree. Expanding a file opens its methods inline;
selecting a method generates an SVG static sequence view alongside the tree. Source controls
open and highlight the cached indexed source. Previous question/answer and call-hierarchy
tools remain available under secondary inspection tools.

135 Rust tests, 53 UI tests and 19 ACP runner regressions pass. Formatting, clippy with
`-D warnings`, and build pass. Remote CI was not run. No provider calls were made.

## Observed browser path

- Loaded the existing revision-2 snapshot: 18 files.
- Expanded `core/atomic-write.js`: six methods appeared under the file.
- Selected `writeProtectedFileAtomic`: six participant lanes and 91 structured steps,
  including branches, await boundaries, cleanup/exception paths and source-backed calls.
- `link` and `rename` remain in distinct branch paths, not an asserted linear runtime trace.
- Clicking a step highlighted its source line and retained the selected method.
- Desktop widths 1200/1440 and mobile width390 had no page-level horizontal overflow.
  The diagram scrolls within a bounded65vh viewport, not a4800px page section.

The large step count is an explicit first-pass limitation: most details remain visible.
Method-importance classification is not implemented. Only standalone console.log/debug
wrappers are hidden by a reversible naming heuristic, while their argument effects remain.
This is not a claim of semantic consequentialness.

## Correctness checks

Nested receiver/argument calls precede their enclosing invocation. Branches and
short-circuit expressions stay guarded. Standard loop ordering is modeled without
unrolling. Returns/throws prevent false straight-line continuation. Cleanup paths are
labeled separately from conditional catch handlers. Computed for-of bindings are
per-iteration. Unsupported loop transfers, patterns, optional chains and class-definition
effects remain explicit boundaries rather than fabricated steps. Bounds preserve warnings
and source-backed call evidence where possible. Implicit getter/coercion effects are not
expanded.

TypeScript documents (`.ts`, `.tsx`, `.mts`, `.cts`) use the JavaScript adapter with the
TypeScript grammar (`tsx` for `.tsx`). Type-only syntax (annotations, type arguments and
parameters, interfaces, aliases) adds no behavior; `as`, `satisfies`, `!` and `<T>x` expressions
walk only their value; a plain typed parameter needs no boundary. An overload, ambient or abstract
signature has no body, so its view is one boundary, "TypeScript declaration has no body;
behavior unknown." Every other TypeScript form the adapter does not model stays a boundary.

The language-neutral DTO and renderer are separated from the JavaScript and TypeScript adapter. For now,
behavior is derived on demand from cached indexed source and measured calls; this does not
read the live workspace or change the persistent graph schema.

[Metadata](sequence-validation.json) · [Desktop](images/file-method-sequence-desktop.png)
· [Mobile](images/file-method-sequence-mobile.png)
