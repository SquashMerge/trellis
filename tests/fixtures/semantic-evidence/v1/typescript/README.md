# TypeScript conformance corpus — native-v16 DRAFT

Seven TypeScript sources for #683 cover interfaces, type aliases, enums, generics,
abstract classes, parameter properties, decorators, overloads, namespaces and
modules, ambient declarations, type-only imports and exports, `import = require`,
and TSX. They exercise the native TypeScript constructs of the contract-v1
TypeScript amendment (Decision 0020).

**This is not the delivered semantic corpus.** `profile:"draft"` skips only
corpus count floors, as for the JavaScript draft. All schema, source, measurement,
coverage, identity and publication checks still apply. There are **no semantic
captures** (Decision 0017 Amendment 7): scenario arrays, semantic artifacts,
dispositions, callable negatives and rooted answers are empty. Amendment 7 sets
the TypeScript floors equal to JavaScript's (ambiguous 20, unresolved 20, type
relationships 20, unsupported documents 0, alias role required); they are
**not certified** here. No disabled family, dropped record or artificial gap is
used to meet a floor.

## Current evidence

- 133 declarations, 44 invocations, 6 control regions and 475 native reference candidates.
- 7 production capability reports; every family is available with no gaps.
- Two independent command runs are byte-identical. Semantic counts remain zero.
- `src/ambient.d.ts` is declaration-only: it measures declarations and reference
  candidates and no calls or controls, and its coverage intent requests only
  `definition`, like the call-free JavaScript `public.js` barrel.

The raw capture comes from Trellis's `trellis.native.syntax@native-v16` extractor
through the #95 `native-capture` command, with UTF-8 byte coordinates. Native
identifier-site candidates carry no inferred semantic role or target. `.tsx`
uses the tree-sitter-typescript `tsx` grammar; other paths use `typescript`.
Native measurement does no type checking, so `tsconfig.json` (`allowJs:false`,
`include` of `src/**/*.ts` and `src/**/*.tsx`) and the dependency-free
`package.json` describe the future semantic project, not native inputs.
`captures/native.json` has SHA-256
`72af4180497b6bbbffdc504d486dd1c5d8514d5ded03d7a4662bc62daf5a0e58`. The compiled
native extractor hash is
`a1c7d0caa1c76e9414ecc028aa89190bcf7d34158b59f019e8671e08e45dacd5`, pinned as
`trellis-native-v16-typescript-test-support` in
`docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json`. It is the final
native-v16 extractor of #683. The #800 review re-pinned it after the extractor
stopped measuring type-only import expressions (`typeof import("m")`,
`import("m").T`) as calls; this corpus has none, so the recapture is byte-identical.

## Reproduce

From the repository root, with an owned target directory and the shared
verification lock; do not run during another worker's measurements:

```sh
export CARGO_TARGET_DIR="$HOME/projects/odin/trellis-targets/typescript-recapture"
/opt/homebrew/bin/gtimeout -k 10s 1800s /usr/bin/lockf -k /tmp/trellis-factory-verify.lock cargo build --locked --features test-support --bin trellis
"$CARGO_TARGET_DIR/debug/trellis" native-capture --describe
/opt/homebrew/bin/gtimeout -k 10s 60s /usr/bin/lockf -k /tmp/trellis-factory-verify.lock "$CARGO_TARGET_DIR/debug/trellis" native-capture --fixture "$PWD/tests/fixtures/semantic-evidence/v1/typescript/fixture.json" --output /tmp/typescript-native-recapture.json
cmp /tmp/typescript-native-recapture.json tests/fixtures/semantic-evidence/v1/typescript/captures/native.json
node tools/semantic-contract/cli.mjs generate --fixture typescript
node tools/semantic-contract/cli.mjs check --fixture typescript
node tools/semantic-contract/test/run.mjs
```

Use a fresh output path: capture refuses overwrite. A changed extractor hash
requires a re-pin and recapture. The command reads registered snapshot bytes,
not expected answers, and never runs repository code.

## Native construct table

Each row cites its source line and a captured native anchor in `captures/native.json`.
The anchors prove syntax and coordinates only; no semantic outcome is claimed.
The C-rows are the construct matrix of the TypeScript amendment. C1, C2, C20 and
C22 to C26 are covered by `src/native_typescript_tests.rs`, not by this corpus.

| Row | Construct | Source | Native anchor ref | Native measurement |
|---|---|---|---|---|
| C3 | abstract class | `src/geometry.ts:21` | `2:r1:sid:v1:2269896ca380b25af31e4033a1a79716` | `type` named `Shape`, `Header.modifiers` contains `abstract` |
| C3/C11 | abstract method | `src/geometry.ts:22` | `2:r1:sid:v1:41af0be0ab8632e26fd249868b5f7f3f` | `method` `area` with `abstract` modifier |
| C12 | decorator before `export` (call) | `src/geometry.ts:74` | `2:r1:occ:v2:4a5487ddd21bb2b7e7141623477a7a0c` | `@sealed()` written before `export` is a child of `export_statement`, so its call is owned by the file module |
| C12 | decorator before `export` (reference) | `src/geometry.ts:30` | `2:r1:occ:v2:23c87624f19573c749cd61ab6bd32939` | bare `@sealed` is a reference candidate, no call |
| C12 | member decorator (reference) | `src/geometry.ts:48` | `2:r1:occ:v2:b04725d5784ed1689f02018fa2bc614e` | bare `@logged` is a reference candidate, no call |
| C12 | parameter decorator (reference) | `src/geometry.ts:49` | `2:r1:occ:v2:a1c7676ca266aa33e06edc3628f8482e` | bare `@tagged` is a reference candidate, no call |
| C13 | parameter property | `src/geometry.ts:33` | `2:r1:sid:v1:5840aeb8c8bb52e7a5d015bea5fc9407` | `field` `width`, parent the constructor, modifiers `public`, `readonly` |
| C13 | parameter property (private optional) | `src/geometry.ts:35` | `2:r1:sid:v1:793470ca398b9f80fb048d9bad59e317` | `field` `label`, modifier `private` |
| C14 | override and accessors | `src/geometry.ts:61` | `2:r1:sid:v1:4a84e0f6e56c232002106f902bf12e6c` | `method` `area` with `override` modifier |
| C14 | getter | `src/geometry.ts:65` | `2:r1:sid:v1:4309a95a45e15d65d7593601bdb018d4` | `method` `side` with `get` modifier |
| C4 | interface | `src/shapes.ts:6` | `2:r1:sid:v1:d10ebbb8d1abae54d1ebaae9ef1fda98` | `type` named `Box`; call and index signatures are not declarations |
| C4 | interface method signature | `src/shapes.ts:8` | `2:r1:sid:v1:86eb27c6b79b3f1b5328c4b714a2900d` | `method` `map` under the interface |
| C5 | enum | `src/shapes.ts:17` | `2:r1:sid:v1:c0e09faf25691bdeead2151f289b42f0` | `type` named `Direction`; members are not declarations |
| C5 | const enum | `src/shapes.ts:24` | `2:r1:sid:v1:0c33acaa0e1d0b4a53022e0552f906b3` | `type` named `Flag`, modifier `const` |
| C6 | type alias | `src/shapes.ts:13` | `2:r1:sid:v1:17bbc52183f99f1e448cb17a041e7ff9` | `alias` named `Pair`, `Header.typeParameters` `A`, `B = A` |
| C9 | generic call | `src/shapes.ts:30` | `2:r1:occ:v2:c6d3b8ef37ff51c49acd5b1f7c9ebf48` | `boxOf<T>(…)` is a call with callee `boxOf` |
| C9 | generic function | `src/shapes.ts:29` | `2:r1:sid:v1:851503928ed62cd3cdbdb87815ac9f8d` | `function` `boxOf`, `Header.typeParameters` `T` |
| C7 | `namespace A.B.C` | `src/space.ts:1` | `2:r1:sid:v1:4fab45489e3bebb004600944213ce456` | `namespace` named `Metric` (rightmost identifier; no `Geo` or `Planar`) |
| C7 | `namespace N` | `src/space.ts:17` | `2:r1:sid:v1:5efc0da060c2c783396bec51de699b7f` | `namespace` named `Tools` |
| C8 | `module N` | `src/space.ts:27` | `2:r1:sid:v1:858877ed1b063324ee33f5b0a760741e` | `namespace` named `Legacy` |
| C10 | function overload signature | `src/overloads.ts:1` | `2:r1:sid:v1:352b871eabb5d306560c8960897554ab` | `function` `combine`, sibling ordinal 0 |
| C10 | function overload signature | `src/overloads.ts:2` | `2:r1:sid:v1:e1e69284732a0eeb3ec53d4adc91cdff` | `function` `combine`, sibling ordinal 1 |
| C10 | function overload implementation | `src/overloads.ts:3` | `2:r1:sid:v1:d47f06de60c0833626e5334382a955f8` | `function` `combine`, sibling ordinal 2 |
| C11 | method overload signature | `src/overloads.ts:17` | `2:r1:sid:v1:4cbd43804856da83221941ac2b369034` | `method` `parse`, sibling ordinal 0 |
| C11 | method overload implementation | `src/overloads.ts:19` | `2:r1:sid:v1:cfe482a14aa2b0ee96572beb8396f432` | `method` `parse`, sibling ordinal 2 |
| C11 | abstract method | `src/overloads.ts:31` | `2:r1:sid:v1:b55e62e5fe801bed610943f69cda6c7d` | `method` `encode` with `abstract` modifier |
| C18 | `declare module "m"` | `src/ambient.d.ts:1` | `2:r1:sid:v1:f05815bb28ba0c87e0aeaf83e00e99b8` | `namespace` named `legacy-clock` (string fragment) |
| C18 | declare function | `src/ambient.d.ts:22` | `2:r1:sid:v1:45978184e11138e1b8c7d445b72917ee` | `function` `currentUser` |
| C18 | declare class | `src/ambient.d.ts:24` | `2:r1:sid:v1:c7135855d3b01ee26fe8a9778518795b` | `type` named `Registry` |
| C18 | `declare global` member | `src/ambient.d.ts:15` | `2:r1:sid:v1:e3a912e2fd43327e97a922ce6e7c149e` | `type` `Window`; `declare global` itself declares nothing |
| C19 | declaration-only document | `src/ambient.d.ts:29` | `2:r1:sid:v1:55b2767b6211def0a5cdc505c7383830` | declarations and references only; no calls or control regions |
| C15 | `import type {a as b}` | `src/imports.ts:1` | `2:r1:sid:v1:414bc678d64187edf39e18f892ced96e` | `alias` named `ShapeBox` |
| C15 | `import {type a as b}` neighbour | `src/imports.ts:2` | `2:r1:sid:v1:7e90ded9cfdc50ac14f8e0bff8dc4657` | `alias` named `Heading`; `type Pair` declares nothing |
| C16 | type-only export | `src/imports.ts:7` | `2:r1:occ:v2:fdc8757ecfce67ad89dc65ed89e54ffc` | no declaration; the specifier is a reference candidate |
| C16 | type-only local export | `src/imports.ts:8` | `2:r1:occ:v2:311a737198aa3cc89da2ba39850e0c73` | no declaration or alias; the specifier is a reference candidate |
| C17 | `import x = require()` | `src/imports.ts:3` | `2:r1:occ:v2:69eeb5d9ec645647a2625d910f98a2d1` | no declaration (documented limit); `fs` is a reference candidate |
| C17 | `import A = N.B` | `src/imports.ts:4` | `2:r1:occ:v2:a1e9ddf9f56899d237754f2a99f38560` | no declaration (documented limit); `Planar` is a reference candidate |
| C21 | JSX attribute call | `src/view.tsx:12` | `2:r1:occ:v2:94412a600882fca15dd2300894786860` | call inside a JSX expression is measured under `Label` |
| C21/C9 | JSX generic call | `src/view.tsx:22` | `2:r1:occ:v2:b131f8fe77169f0a2a8f3cc90b2cc4c0` | generic call inside a JSX child, owned by `Panel` |
| C9 | `<T,>` generic arrow | `src/view.tsx:8` | `2:r1:sid:v1:ecd3ed48e4c3ac903afd8fa50731e181` | `variable` `identity` holding the arrow; the arrow is an `anonymousFunction` |

## Amendment-table notes (TypeScript grammar limits)

These two facts come from the pinned tree-sitter-typescript 0.23.2 grammar and the
contract's owner rule. They are recorded here for the contract-v1 TypeScript
amendment table; the corpus deliberately avoids the first so that no artificial
gap is needed.

1. **`export type * from "m"` is not parsed.** tree-sitter-typescript 0.23.2 accepts
   only `export type { … }` (`common/define-grammar.js:320-331`), not the
   `export type *` form. A document containing it gets a recovered-parse gap
   (partial coverage with a diagnostic) instead of complete measurement. This
   corpus contains no such statement, so every report stays complete.
2. **A decorator written before `export` is owned by the file module.** In
   `@sealed() export class M`, the decorator is a child of `export_statement`, so
   its call is owned by the enclosing file module, the innermost executable
   container (contract-v1 owner rule). Decorators on non-exported classes, and on
   members and parameters, are owned by the class or method. `src/geometry.ts`
   shows the exported form (`@sealed()` before `export class Circle`, a
   module-owned call; bare `@sealed`, `@logged` and `@tagged` are reference
   candidates only).

## Toolkit limit: class-owned calls

The corpus authors no call that production attributes to a class: a member
decorator call such as `@logged("scale")`, or a static or instance field initializer
call such as `new Square(1)`. `record-check/measurement.mjs` accepts only module,
function, method, constructor and anonymous-function owners for calls and
controls, so a class-owned call is rejected (`MEASUREMENT.OWNER`). The production
extractor does emit these calls with the class as owner. The corpus therefore
uses bare `@logged` and a literal static field. Lifting the toolkit restriction
needs a change outside this corpus.

## Future work

Semantic captures (scip-typescript 0.4.0 covers JavaScript and TypeScript),
scenario annotations, rooted answers and the full floors await the final corpus
change, which sets `profile:"corpus"`. Never use `-infer-tsconfig` for that capture;
preserve the explicit project configuration and hashes.
