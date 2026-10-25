# Decision 0004: two-step class catalog (per-file extraction, per-revision composition)

- **Status:** approved by the owner on 2026-10-01; ratified when this record merges. It changes normative text in `../../class-diagram-contract.md`. It changes no deployed schema by itself; implementation follows in #67 (see [Implementation impact](#implementation-impact)).
- **Scope:** how the Java/Python class catalog (`Catalog`, `src/classes.rs`) is built and persisted. The class DTOs, IDs, relation kinds, scoped-matching rules, HTTP API and class-diagram behaviour are unchanged.
- **Compatibility: none.** Pre-release, the owner requires no backward compatibility. Catalog output may differ from the current build **only** when a workspace-wide cap is reached (see [Behaviour change](#behaviour-change)).

## Problem

`Catalog::build` is one pass over every Java/Python file in path order. Each file's extraction draws on budgets shared by the whole workspace:

| Shared budget | Constant |
|---|---|
| total class source | `TOTAL_BYTES`, 256 MiB |
| class declarations | `CLASSES`, 20,000 |
| member and relation records | `RECORDS`, 250,000 |
| detail text | `OUTPUT_TEXT`, 64 MiB |
| declaration registry text | `REGISTRY_TEXT`, 32 MiB |
| input size | 100,000 files / 1,000,000 symbols |

It also shares the catalog-wide `warnings` list (deduplicated, at most 100) and `truncated` flag. So a file's output depends on how much budget earlier files used. A file that grows can truncate or drop classes in a later, unchanged file.

#67 publishes native changes by writing only what changed. A pinned read must be byte-identical to a full rebuild, classes included, and single-file updates must meet a p95 (≤2 s medium, ≤5 s large) that includes class projection. Under the one-pass build, the only exact way to update classes is to re-parse every Java/Python file on every publication. That is O(workspace) work per edit.

`resolve()` binds no cross-file targets today: every relation is `unmatched`, with no target or candidates. So the shared budgets and warnings are the **only** cross-file dependency in the catalog.

## Decision

The catalog is built in two steps. Every limit is read from one `Limits` value. Production uses the constants above, and the fixture tests below use small limits. The warning strings below are shown with production values; their numbers are formatted from the active `Limits`.

### 1. Per-file extraction

- **Inputs.** A file's extraction is a function of only:
  - the file's bytes, path and language;
  - the file's measured `Symbol`s, including their captured SCIP display labels;
  - the **per-file** limits: `FILE_BYTES` (2 MiB), `FILE_CLASSES` (1,000), `FILE_REFS` (8,192), `MEMBERS` (256 per class), `VISITS` and detail visits (100,000), `DEPTH` (64), `TEXT` (2,048 bytes per string), and the type-parameter limits (256 per declaration and per scope).
- **No workspace budget.** It reads no workspace-wide budget and is never cut by one. `tick`, `details`, `reserve_text` and `reserve_detail_text` keep only their per-file checks. The registry and detail charges are **counted**, not refused.
- **Per-file messages are unchanged,** including `"{path}: class declaration limit reached (1000/file, 20000/catalog)"` when `FILE_CLASSES` is hit. That keeps below-cap output identical.
- **Result `F`.** Extraction produces:
  - `F.source_bytes`: the file's byte length. A file skipped by `FILE_BYTES` still has one, as today.
  - `F.registry_bytes`: the sum of every registry-text charge the extraction made. That is binding name plus value, class `id + name + path + qualified name`, and type-parameter scope text ×2. A file that declares **no** class still charges its bindings.
  - `F.warnings`: the file's own limit warnings, in emission order.
  - `F.truncated`: whether any per-file limit truncated output.
  - `F.registry_incomplete`: whether a per-file limit that today calls `registry_limit` fired (for example, no unique measured class symbol, an over-long qualified name, a type-parameter limit, or the work limit).
  - `F.classes`: the file's classes in `(start_byte, id)` order. Each class carries its declaration and its **detail items** (fields, methods, relations and detail type-parameter text) in extraction order. Each item has `records`: 1 for a member or relation, 0 for type-parameter text, and `text`: the bytes that `reserve_detail_text` charges.
- **Reuse.** `F` is stored with the document's **graph projection**, whose identity covers the document version and the file's measured `Symbol`s. `F` is reused exactly while that graph projection is reused, and recomputed whenever it changes. This includes an unchanged source file whose captured SCIP labels change.

### 2. Per-revision composition

- **Input limit.** If the revision has more than 100,000 files or 1,000,000 symbols, the catalog is empty, `truncated = true`, and `warnings` holds only the input-limit message, with no closing warnings. This is today's behaviour.
- **Otherwise,** walk the Java/Python files in ascending byte order of path. Start with counters `S` (source), `T` (registry text), `N` (classes), `R` (records) and `X` (detail text) at 0, and latches `registry_open` and `detail_open` set to true.
- **For each file `F`:**
  1. If `registry_open` is false, skip `F`. It contributes no classes and no warnings.
  2. **Registry caps.**
     - If `S + F.source_bytes > TOTAL_BYTES`, emit `"Class catalog source limit exceeded (256 MiB)"`. Otherwise, if `T + F.registry_bytes > REGISTRY_TEXT`, emit `"Class declaration registry text limit reached (32 MiB)"`. In either case, set `registry_open` false, mark the registry incomplete, and skip `F` and every later file.
     - Equality is admitted.
     - An excluded file's own warnings never enter the list.
  3. **Admit the file.** Add `F.source_bytes` to `S` and `F.registry_bytes` to `T`. Append `F.warnings` in order. If `F.truncated`, set the catalog's `truncated`.
  4. **For each class `c` of `F`, in order:**
     - If `N = CLASSES`, emit `"Class catalog declaration limit reached (20000)"`, set `registry_open` false, mark the registry incomplete, and stop. The rest of `F` and every later file are excluded; `F`'s warnings from step 3 stay.
     - Otherwise, admit `c`'s declaration and add 1 to `N`.
     - For each item `i` of `c`: admit it only if `detail_open`, `R + i.records ≤ RECORDS` and `X + i.text ≤ OUTPUT_TEXT`; then add `i.records` to `R` and `i.text` to `X`.
     - At the first item that fails, set `detail_open` false and emit one detail warning. If `R + i.records > RECORDS`, it is the record warning `"Class detail limit reached (250000 records / 64 MiB text / 100000 visits per file); declaration discovery continues"`. Otherwise it is the text warning `"Class detail text limit reached (64 MiB); declaration discovery continues"`. That item and every later item, in this class and every later class, are dropped. A class that loses at least one item is `truncated`.
     - Declarations keep being admitted after `detail_open` closes.
- **Warnings list.** Warnings are added in walk order and deduplicated by exact string. The first 100 distinct entries are kept and later ones are dropped silently.
- **Closing warnings.** After the walk, these follow, outside the 100-entry limit and in this order:
  1. `"Declared types are terminal source text…"`, if any class was admitted;
  2. `"Incomplete class declaration registry: some declarations may be absent."`, if the registry is incomplete. That is either a registry cap above, or an admitted file with `F.registry_incomplete` set.
- **Catalog `truncated`** is set when any cap above fires, or when an admitted file's `F.truncated` is set.
- **Output.**
  - Classes come out in `(path, start_byte, id)` order, and relations in `(path, start_byte, id)` order, deduplicated by `id`, as today.
  - Composition yields one **class projection per document** (its admitted classes and relations) plus the revision's catalog `warnings` and `truncated`.
  - Composition reads only the `F` values. It never re-parses.

`Catalog::build(files, nodes, cancel)` is defined as `compose(extract_file(f) for every file)` with production limits. #67's full-snapshot parity for classes is measured against that definition, not against the current one-pass output.

### Equivalent composition from compact summaries (#300)

Composition need not decode every stored `F` when compact per-file summaries prove that no workspace-wide cap is reached. Each summary records the effective source, registry, declaration, record and detail-text charges, intrinsic flags and ordered warnings. Invisible detail-text items still charge their budgets. Summary composition must reproduce the full composition's warnings, warning order, deduplication and flags exactly.

The selected manifest records whether each class projection contains the complete per-file output of `F`, including intrinsic class flags. This is selection admission state, not `!F.truncated`. When all global budgets fit, an unchanged complete projection may be reused by CID without rebuilding its DTOs or hash. A previously incomplete selection must recover its full per-file rows from the immutable `F` before publication. Thus falling below a cap immediately restores previously excluded output; truncation is not sticky.

The first #300 implementation retains the existing full composition path whenever a global cap may be reached. Its cutoffs and warning rules above are unchanged. Compact summaries are stored with the immutable graph projection; they do not replace `F` as the recovery input or add a second cache.


## Alternatives rejected

- **Keep the one-pass output exactly, and replay or re-parse to reproduce it.** Reproducing today's mid-class budget cutoffs and warning order on a delta needs every earlier file's extraction state. In practice that means re-parsing every Java/Python file, or replaying stored per-file extraction traces, on every publication. That is O(workspace) work per edit, against a p95 that includes class projection.
- **Exclude classes from parity or from the p95, or let class rows lag behind.** That contradicts #67's acceptance criteria.

## Consequences

- **Defined parity.** Delta publication and the full rebuild apply the same normative rules. The tests below check them against an independent build and against frozen expected outputs, not just against each other.
- **Delta writes.** A publication re-extracts only files whose graph projection changed: an edited file, or an unchanged file whose captured SCIP labels changed. Composition then rewrites only the class projections whose content changed: normally just those files'. Another document's projection changes only when a cap boundary moves across it.
- **Storage.** Per-file results live on the graph projection and per-document class projections are shared across revisions, so an unchanged file's class rows are stored once.
- **No resolution is added.** Every relation stays `unmatched`, as today. Binding relation targets across files would be a separate semantic decision.

## Behaviour change

Below every workspace-wide cap, output is identical to the current build. Frozen fixtures check this (see the tests below). At a cap, the rules above replace today's. Today:

- An overflowing registry-text charge drops one binding or class, and then every later node in the file and every later file stops, with a per-file "work limit" warning.
- Detail budgets are consumed in traversal order, interleaving nested classes.

Under this decision, registry caps exclude whole files, `CLASSES` cuts between classes, and detail caps cut at the first failing item, in class order. Nothing depends on the old cut points.

## Required tests (#67)

1. **Frozen below-cap outputs.** Before `classes.rs` changes, record the current one-pass `Catalog::build` output, byte-for-byte JSON, for the existing class fixtures and a below-cap sample of the #72 cohorts, and check it in. The new implementation must reproduce it exactly.
2. **Normative cap fixtures** with small `Limits` and checked-in expected outputs, written from the rules above and not generated by the implementation under test. For each of `TOTAL_BYTES`, `REGISTRY_TEXT`, `CLASSES`, `RECORDS`, `OUTPUT_TEXT` and the input limit:
   - **Boundary:** exactly at the limit (admitted) and one unit over (cut).
   - **Registry text without classes:** a file with no class whose bindings cross `REGISTRY_TEXT`.
   - **Detail cut inside a class:** a class whose items cross `RECORDS` and, separately, `OUTPUT_TEXT`. It keeps its declaration and its earlier items, is `truncated`, and later classes keep declarations without items.
   - **Excluded file's warnings:** they are absent from the list.
   - **Warning limit:** deduplication and the 100-entry limit, with the closing warnings after it in order.
   - **Moving cut:** growing an earlier file moves the cut in a later, unchanged file, and delta publication rewrites exactly that file's class projection.
3. **Independent oracle.**
   - #67's full-snapshot oracle builds the catalog in a fresh, isolated build. It re-extracts every file from the authenticated captured source and the revision's measured graph, and reads **no** stored `F`, graph projection or class projection.
   - It compares every pinned per-document class row, the order, the catalog `warnings` and `truncated`, byte-for-byte, with delta output after each #67 parity scenario.
4. **Cache invalidation.** An unchanged source file whose captured SCIP label changes gets a new graph projection, a recomputed `F` and the class projection that follows from it at the new revision, while an older retained pin still reads its exact previous rows.
5. **Workspace independence.** The same per-file inputs (bytes, path, language, measured `Symbol`s) give the same `F` in any workspace.

## Implementation impact

- **`src/classes.rs`.** Split into a per-file extraction (`extract_file`) and composition (`compose`), both taking `Limits`. `Catalog::build` becomes `compose` over `extract_file`. The global counters and refusals move out of extraction and into composition.
- **`src/store.rs` (#67 v8).**
  - `F` is stored on the document's graph projection and reused or recomputed with it.
  - Per-document class projections (`class_projections`, `classes`, `class_relations`) are named by each revision's manifest.
  - The catalog `warnings` and `truncated` move to the revision header.
  - Composition runs before the publication transaction, and the transaction writes only new projections.
- **`docs/class-diagram-contract.md`.** Its cap rule now points here.
- **#67 plan.** This is its `class-composition` slice. Selected reads serve a path's stored projection for the pinned revision without recomputing composition (amended 2026-10-23, #261: selected-document read attestation removed by owner ruling).
