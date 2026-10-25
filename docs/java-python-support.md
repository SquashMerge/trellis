# Java and Python support

Trellis indexes `.java` and `.py` files with native tree-sitter adapters. They use the same
file → method → static sequence → cached source workflow as JavaScript and Rust.

## Included

- Java lexical classes, interfaces, enums, records, constructors, methods and lambdas.
- Python lexical classes, functions, methods, async functions and lambdas.
- Source-linked calls, lexical owners/regions, UTF-8 byte ranges, recovered-syntax diagnostics,
  cancellation and atomic snapshot publication. IDs distinguish nested shared-start calls.
- Evaluation-order-aware basic calls, conditions, short circuits, loops and explicit exits.
- Original measured call IDs/ranges and named unresolved source hints in the shared diagram.
- Source snapshots remain readable after live source changes. Index explicitly to refresh.

Java/Python calls remain **unresolved**. Lexical class ownership is not compiler type or dispatch
resolution. JavaScript SCIP evidence is never applied to either adapter. Show all does not expand
unresolved or third-party behavior. No implicit constructors, generated getters or return arrows
are fabricated.

## Conservative boundaries

Java try/catch/finally/resources, switch/patterns, synchronized, do/labeled/assert statements,
method references, array construction and basic for-loops with continue are not expanded.
Annotations, Lombok, class initialization, implicit exceptions and generated methods are not run
or inferred. Java 25 syntax is not claimed universally supported; validate required syntax with synthetic fixtures.

Python comprehensions, generator/yield behavior, try/else/finally, with, match, async for,
loop else, augmented/chained assignment, annotation-only non-simple targets, splatted arguments,
chained comparisons, f-strings and other unsupported forms use explicit boundaries. Valued
local annotations preserve RHS → target address → write order without evaluating annotations. Nested definition effects,
decorators/defaults and class execution are separate from the selected callable body. Async and
generator body diagrams mean possible execution when awaited/iterated, not at call time.

Unknown transfers and resource cutoffs do not make following operations unconditional. Sequence
output is bounded to 200 steps, 20 participants and depth 24, with explicit partial/unknown state.

## Read-only operation and scope

Indexing never runs Gradle/Maven, annotation processors, Python imports, build scripts, package
installation or providers. Build/test commands for Trellis itself are separate development work.
Supported root configuration files are hashed as data; nested/transitive compiler configuration
and package environments are not attested.

Ignored environments/build output and symlinks are excluded. Conventional Java source packages
named `build`, `target` or `dist` remain indexable under `src/main/java`, `src/test/java` and
`src/testFixtures/java`; artifact ancestors and ignore rules still apply. Hidden directories and
other `build` directories remain excluded, including Python helper scripts located there.

Automatic Maven/Gradle/pip library catalogs and Java/Python semantic resolution are **not** part
of this slice. The automatic library panel still supports Rust/Cargo only. Angular/TypeScript
files are not silently treated as JavaScript.

## Inspect another repository

Run Trellis from its own directory. Indexes use fixed per-user storage; retired saved records remain in place and ignored;
`--state-dir` is removed. Put the required private token outside the inspected checkout:

```sh
mkdir -m 700 -p "$HOME/.trellis-private"
target/debug/trellis index --workspace /path/to/repository
target/debug/trellis serve --workspace /path/to/repository \
  --token-file "$HOME/.trellis-private/token" --bind 127.0.0.1:8879
```

The file tree and index use the same workspace by default. Serving does not run the application
or automatically index. Provider flags are intentionally omitted.
