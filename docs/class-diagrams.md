# Class diagrams and related classes

Class diagrams use the indexed Java, Python, JavaScript and Rust snapshot. They do not load or execute the inspected
application, run package tools, fetch dependencies, or send source to a provider.

## Use the view

- Right-click an indexed `.java`, `.py`, `.js`, `.mjs`, `.cjs` or `.rs` file and choose **Show class diagram**.
- Right-click a method and choose **Show enclosing class diagram**, or use its **Class diagram** button.
- Alternatively open **Classes**, search a class or qualified name, and choose a result.
- The view starts with the focus and its indexed inheritance hierarchy: ancestors, subclasses and
  implemented interfaces. Traversal does not expose unrelated siblings through a shared ancestor.
- Right-click a class and choose **Show related classes**, select other type references, then
  **Add selected classes**. A visible hierarchy class does not need to be manually added first.
  The chooser uses the current bounded result, not an exhaustive workspace graph.
- **Members** reveals fields, methods, qualified name and path. Click a method name to open its sequence.
  Click a type hint, right-click a member, or use its `…` button to navigate recorded type candidates
  and the member's method. See [source and member navigation](member-navigation.md).
- **Show all returned classes** reveals the bounded API result; **Show hierarchy and selected classes**
  restores inheritance plus the chosen associations. **Change class** reveals search results. **Focus this class** starts over.
- Use **Read class source** for the cached declaration. Selection, member disclosure, menus and
  related-class exploration do not read source.

The visible `…` menu works on touch. A focused class supports Shift+F10 / the Context Menu key.
Menus support arrow keys, Home/End and Escape. Diagrams scroll inside the canvas on narrow screens.
Switching between Classes and Sequence retains the current class view until the workspace/index changes.

## What the links mean

Class cards show direct declared fields and methods. Edges come from explicit inheritance and
field/parameter/return type syntax. Both incoming and outgoing references can reveal related classes.
Dashed edges are **scoped syntax candidates**, not compiler-resolved types, runtime object relationships,
method dispatch, ownership/composition or multiplicity. No generated methods are invented.

Matching uses declaration, package/import and lexical context. Ambiguous or unmatched names stay
terminal hints; **View options → Include unmatched type hints** reveals them. They cannot be expanded as known classes.
Python module names follow workspace-relative source paths, without guessing runtime import roots.
Wildcards, inferred local types, dynamic factories, evaluated aliases, function-body/conditional/anonymous
classes and third-party catalogs are not resolved.

JavaScript boxes come from named ES `class` declarations at module level, including `export class`
and `export default class`. Cards show direct fields and methods, `static` ones included, in source
order. `#private` members are listed but cannot open a sequence, because they are not measured
declarations. `extends Name` or `extends ns.Name` is recorded as a terminal hint; other heritage
expressions, such as a mixin call, record nothing. Plain JavaScript declares no field, parameter or
return types, and JSDoc is not read, so a JavaScript class has no type hints. Class expressions,
prototype assignment, factories and object-literal "classes" are not shown. A JavaScript qualified
name is the workspace-relative path without its extension, with `/` written as `.`, then the class
name (`web/shell.js` → `web.shell.Shell`); bundler and package resolution are not modelled.

Rust boxes are `struct`, `enum`, `union` and `trait` items (the annotation shows `rust · struct`,
`rust · trait` and so on). Structs list named fields, or `0`, `1`, … for tuple fields, with their
declared types; enums list their variants, with the payload as the type hint. A trait lists its
methods, default methods included. Methods of every inherent `impl Type` block, in any file, are
merged into that type's box, in path then source order; each keeps its own path and range.
`impl Trait for Type` records an `implements` hint on the type and does not add the trait's
methods; `trait A: B` records `extends`; `#[derive(...)]` names are `derives` hints. Field and
signature types become `field`, `parameter` and `returns` hints; generic parameters and `Self`
never do.

An `impl` block's type is matched only through the file's own `mod` and `use` scopes, never by
bare name across the workspace: `impl Account` binds to an `Account` declared in the same module
or imported by `use`, `impl crate::model::Account` to that path. A `use super::*` glob is followed
only into a module of the same file; other globs, re-exports, type aliases, external types and
cfg-duplicated declarations leave the impl unattributed. Blanket impls (`impl<T: X> Y for T`) and
impls for references, tuples or trait objects are not attributed and add a notice. Macro-generated
items, derived impls and `#[path]`-mounted modules are not shown. Qualified names follow the
conventional Cargo layout: `src/a/b.rs` is `a::b`, `src/lib.rs` and `src/main.rs` are the crate
root, and a crate outside the workspace root, a `src/bin` target or a file outside `src/` (tests,
examples, `build.rs`) is prefixed with its directory or file path (`tests/it.rs::Fixture`).

Repeated same-kind references between the same endpoints use a deterministic representative edge in
the diagram. Its ID and source range are real. The full reference catalog remains stored. The relationship
evidence disclosure identifies every returned reference. The renderer bundles these references into
one arrow per ordered class pair, labelled with its declaration kinds; opposite directions stay distinct.
Original returned IDs, ranges and candidate evidence remain in the folded disclosure. Read the class
source for omitted repetitions and declaration detail.

## Bounds and continuity

Views show at most 24 nodes / 64 edges and accept at most 12 additional connected expansion roots.
Fields/methods, source traversal, registry text, detail text, references and response sizes are bounded.
Partial results have visible notices. Detail truncation does not suppress otherwise valid candidate links;
incomplete declaration discovery disables unique matching rather than inventing certainty.

Class projections are stored atomically with the source snapshot. Schema 3 adds projection tables and
preserves cached source and durable views/notes. Old snapshots show **Index workspace** guidance until
explicitly reindexed; class requests never scan live source. Workspace/session/revision guards invalidate
old menus, callbacks, diagrams and pending source reads. Provider packets and their authorization budgets
are unchanged. Class layout itself is not saved as a durable view in this slice.

## API

- `GET /api/classes?revision=N&q=NAME&path=RELATIVE_PATH&offset=0&limit=100`
  lists indexed declarations; omit `path` for workspace search.
- `POST /api/class-diagram` with `{seed, expectedRevision, expanded: [], includeUnmatched: false}`
  returns a bounded diagram. `seed` may be a class or a method with a lexical enclosing class.
  Optional `includeHierarchy: true` prioritizes bounded transitive ancestors/descendants; the UI sends
  this flag. Omission retains the original neighborhood API behavior. It does not change stored certainty.

`GET /api/classes` also returns `supportedLanguages`, the source languages the catalog extracts
(`["java","python","javascript","rust"]`). A client shows class views for a checkout that has indexed
files in one of them, and each returned class carries its own `language`.

Both endpoints require existing bearer/Host/Origin guards. Responses carry their snapshot revision,
warnings, truncation and `requireIndex` state. Unsupported or invalid requests fail explicitly.
