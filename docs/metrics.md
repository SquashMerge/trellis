# Metrics v1

`metricsVersion` is `metrics:v1`, independent of the native evidence version. Metrics are computed from selected pinned source and native facts, not persisted. A supplied pin is the complete `(indexGeneration,indexRevision)` pair; missing means the admitted current head. A stale, incomplete or wrong-generation pin never falls back to head. Invalid selected source/facts abort the whole result.

## Callable role selection

Metrics options accept exactly `role=production|all|test`; CLI uses `--role`. Omitted API/CLI
options default to `all`. The Viewer and Architecture explicitly request `production` by default;
their unchecked **Include tests and fixtures in metrics** controls select `all` when checked. Test-only
selection is available through API/CLI, not a third browser control. The payload echoes `role`.

Role comes directly from each pinned native `Declaration.role` under
[Decision 0011](semantic-evidence/decisions/0011-test-production-declaration-role.md), for Rust,
Java, Python and JavaScript. It is syntax/convention classification, not test-runner discovery.
Rust inline `#[cfg(test)]` callables inherit test; `#[cfg(feature = "test-support")]` by itself
remains production. Browsers do not classify filenames or source text. Missing or invalid stored
roles are typed index corruption, not silently production or partial results.

Scoring and binding use the same selected pinned source/native rows before role filtering. The
selected callable population supplies every aggregate, declaration output and highest-cognitive
ranking. Source-only memo entries remain role-independent and bind current stored roles on reuse.
Each file row also carries `roleCounts: {production, test}` for all measured callables at that pin,
independent of the role selection; `declarationCount` is the selected count. Manifest-selected
file/directory rows remain present with count zero and null numeric aggregates when the selected
population is empty. A declaration selector excluded by role returns `not_found`.

## Workbench heat (#395)

Architecture, Treemap and the shared Diagrams explorer (including beside Sequence) use
one fixed scale. Sequence itself does not invent metric values for call steps.
`max` and `mean` use these per-callable bands; `mean` is the existing API name for average.
Intervals include fractional averages, not just integer scores.

| Metric | Low | Moderate | High | Hotspot |
|---|---|---|---|---|
| cognitive | ≤5 | >5–15 | >15–30 | >30 |
| cyclomatic | ≤5 | >5–10 | >10–20 | >20 |
| maxNesting | ≤2 | >2–3 | >3–4 | >4 |
| sloc | ≤30 | >30–60 | >60–120 | >120 |
| parameters | ≤3 | >3–5 | >5–7 | >7 |
| fanOut | ≤7 | >7–15 | >15–30 | >30 |
| maintainabilityIndex | ≥40 | ≥20–<40 | ≥10–<20 | <10 |
| halstead.volume | ≤500 | >500–1000 | >1000–2000 | >2000 |

Maintainability is higher-is-better; heat severity is inverted, values are not.
`loc`, `commentLines`, `fanIn` and `exits` are informational and neutral by default.
`sum` is neutral and labelled “relative totals; no callable risk bands”. It does
not silently use quartiles. **Relative**, off by default in each view, explicitly
switches to quartiles of the visible finite values and labels the legend “Relative
to visible set”. Ties remain together: sorted cuts use
`min(n-1, floor(n*p))` for `p=0.25,0.5,0.75`, with strict `value > cut` comparisons.
Null and nonfinite values are unavailable, never zero. Changing filters cannot
change a fixed colour. Changing Relative or colour does not fetch metrics.

### Workbench population

Browser requests explicitly send `workbench=true` on `/metrics` and `/architecture`;
the payload echoes the choice. HTTP accepts only `workbench=true|false`. Authored rules
are live configuration, not pinned code; Architecture uses the same captured configuration
for assignments, the rules hash and metric exclusions. Omitted API/CLI options retain the existing full
population (`workbench=false`, `role=all`). This is a view population choice, not
a new stored role, metric definition, index format or source-only memo key.

The default browser `role=production` uses stored declaration roles and additionally
excludes fixture inputs under `tests/fixtures/` and
`tools/synthetic-cohorts/templates/`, plus files assigned by the existing layer
matcher to authored `display="hidden"` layers. The **Include tests and fixtures in
metrics** checkbox requests `role=all` and restores those tests/fixtures/hidden-layer
contributions. Expanding a layer, quiet-layer folding, path filtering and client
visibility do not change this selected callable population. Missing rules add no
layer exclusions; explicit fixture roots still apply. Invalid or unreadable rules, including
ambiguous layer membership, also leave metrics heat available without layer-based exclusions.
The metrics payload carries `layerExclusionsWarning`, shown by the explorer and Treemap;
stored-role and explicit fixture/vendor exclusions still apply. Architecture retains the
located typed rules error instead of showing a partial policy diagram.

Under `workbench=true`, `web/vendor/` is the clear vendored/generated path rule:
its files, including the bundled Pele renderer, never contribute metrics, even
with tests included. First-party Pele adapters outside that directory remain
eligible. This rule is documented and exact; it does not guess from filenames or
layer names. No broad `web/` exclusion applies.

Filtering happens once before file, directory, top-cognitive and declaration folds.
File rows remain with physical lines and all-role `roleCounts`; an excluded
population has count zero and null metric leaves. An optional file `excludedReason`
(`fixture`, `vendored` or `hidden_layer`) identifies deliberate whole-file exclusion.
Displayed hidden-callable counts use all-role counts minus selected count, without
double-counting test fixtures. Files remains an independent complete indexed catalog,
so excluded source is still reachable. Architecture references, assignments and
policy totals are unchanged. All selected source/fact validation remains in force.

## Declaration boundary and lines

Rows represent native callable declarations (`function`, `method`, `constructor`, `anonymousFunction`) and retain native `syntax_id` as `sid` and the native half-open byte `range`. Types, modules and fields are not callable rows. Exclude represented nested callables' tokens, branches and calls from their enclosing callable. An unrepresented nested callable remains in its nearest represented parent. Physical lines may overlap and aggregate counts intentionally double-count them. Production-selected callable-range LOC/SLOC sums are not production physical file lines; this API does not measure physical production LOC. `loc` counts physical lines intersecting a nonempty half-open byte interval: LF and CRLF each end a line, and a final terminator does not invent an empty line. `sloc` counts intersecting lines with non-comment, non-whitespace tree-sitter tokens. `commentLines` counts each intersecting line containing comment syntax, including mixed code/comment lines and block-comment continuation. Text that looks like a comment inside a string is not a comment. An empty interval gives zero for all three.

## Complexity and size

`cyclomatic` starts at 1. Add one per if/elif, loop, non-default switch or match arm, catch/except, ternary and short-circuit AND/OR operator. Else, default, break, continue, return, throw, and the switch/match heading add zero. Rust `if let` and `while let` contribute one each. Do not expand macros or double-count arms/labels. Rust `?` contributes zero to cyclomatic and cognitive and is not an explicit exit (a deliberate divergence from oracles that count fallible propagation as a branch).

`cognitive` adds `1 + structural depth` for an if, loop, switch/match, catch or ternary; each structural body increases depth once. Else, else-if and elif add a flat one each; an else-if continuation gets exactly one increment, not an additional `else` increment, and does not nest as another branch. Case arms themselves add no structural increment. A lambda/closure adds zero and enters at depth +1; this includes test-framework callbacks. Each maximal same-operator Boolean sequence adds a flat one. Changing AND to OR, grouping or a branch boundary starts a new sequence. A labeled break/continue adds one. A syntactic self-call by the current named callable adds one per call; qualified calls and anonymous callbacks are not inferred recursive. `maxNesting` is the maximum *stored structural control-region* depth (top-level 1), excluding lambda depth; no control regions gives 0.

`parameters` counts explicit formal entries, including self/receiver and rest/varargs once, but not implicit receivers. `exits` has counts of explicit returns and Java/JavaScript throws or Python raises; Rust implicit tail expressions and `?` do not count. `fanOut` counts stored call **sites** owned by this native callable, including unresolved and repeated sites; it is not distinct callees. `fanIn` is always `{"value":null,"unavailable":"call resolution not yet produced (#40/#68)"}`; never claim zero.

## Halstead and maintainability

Count lexical leaves of the callable body only: signature names, parameter and return types, header keywords, and bodyless interface method headers contribute no Halstead tokens. Full declaration ranges still determine `loc`, `sloc` and `commentLines`. Exclude whitespace, comments, decorators/attributes, and represented nested callable bodies. Identifier/property and number/string/character/boolean/null literals are operands keyed by exact source spelling; an interpolated/template literal chunk is one operand and its expressions are counted normally. Other keyword/operator/punctuation leaves are operators keyed by spelling, excluding delimiters `()[]{},;:` and layout. Assignment, comparisons, arithmetic, bitwise, Boolean, arrows and member-access punctuation are operators. Significant keyword examples (not exhaustive):

| Language | Keywords and literal rules |
| --- | --- |
| Java | `new instanceof return throw if else for while do switch case catch finally break continue`; `true false null` are literals. |
| Python | `and or not is in if elif else for while match case try except finally return raise lambda await yield`; `True False None` are literals. |
| JavaScript | `new typeof instanceof in delete void await yield return throw if else for while do switch case catch finally break continue`; `true false null` are literals; `undefined` is an identifier. |
| Rust | `if else for while loop match return break continue as await move ref mut let ?`; `true false` are literals; `None` is an identifier. |

Let `n1,n2` be distinct operator/operand spellings and `N1,N2` their occurrence counts. `n=n1+n2`, `N=N1+N2`, volume `V=N*log2(max(n,1))` (zero when N=0), difficulty `D=(n1/2)*(N2/max(n2,1))` (zero when n2=0), effort `D*V`, and estimated bugs `V/3000`. Counts are exact integers; round derived f64 values to six decimal places, normalize negative zero. Nonfinite values or overflow cause typed bounded-work failure. Normalized `maintainabilityIndex` has no comment weighting: `clamp((171 - 5.2*ln(max(V,1)) - 0.23*cyclomatic - 16.2*ln(max(loc,1)))*100/171,0,100)` rounded to six decimals.

## Grammar mapping

| Grammar | Structural/branch nodes | Lambda | Boolean |
| --- | --- | --- | --- |
| Java | `if_statement`, `for_statement`, `enhanced_for_statement`, `while_statement`, `do_statement`, `switch_expression`/`switch_statement`, `switch_block_statement_group`/`switch_rule`, `catch_clause`, `ternary_expression` | `lambda_expression` | `&&`, `||` |
| Python | `if_statement`, `elif_clause`, `for_statement`, `while_statement`, `match_statement`, `case_clause`, `except_clause`, `conditional_expression` | `lambda` | `and`, `or` |
| JavaScript | `if_statement`, `for_statement`, `for_in_statement`, `while_statement`, `do_statement`, `switch_statement`, `switch_case`/`switch_default`, `catch_clause`, `ternary_expression` | `arrow_function`, `function_expression` | `&&`, `||` |
| Rust | `if_expression`, `for_expression`, `while_expression`, `loop_expression`, `match_expression`, `match_arm`, `closure_expression`, `try_expression` (`?` adds zero) | `closure_expression` | `&&`, `||` |

Reject the entire request with typed `invalid_source_syntax` if *any* selected pinned source parses with a tree-sitter `ERROR` or `MISSING`, even outside a callable. A valid file with zero callables succeeds. Cancellation and deadlines are separate `cancelled`/`deadline_exceeded`, not syntax errors.

## Aggregation and unavailability

Select from canonical order `loc,sloc,commentLines,cyclomatic,cognitive,maxNesting,halstead,maintainabilityIndex,fanOut,fanIn,parameters,exits`. For every numeric leaf (including Halstead derived values, MI and exits), `sum`, `max` or `mean` folds declaration scores, never means of file means and never recalculates derived metrics from aggregate inputs. Round fractional means to six decimals. Empty aggregates have null numeric leaves; fanIn remains unavailable. An unavailable constituent yields null with its specific reason, never zero substitution. An empty revision has empty arrays; an existing empty file or directory succeeds with count zero. Revision and directory process every selected declaration but omit declaration rows; file returns all its declarations, and declaration returns its uniquely selected `(sourceSetId,path,sid)` row. File and directory rows follow byte ordering. Revision includes all files and directory ancestors, including each source-set root. No on-disk empty-directory inference. The complete serialized success envelope has a 64 MiB limit and fails atomically with `response_too_large` if exceeded.


## Browser transport and highest cognitive callables

`GET /api/checkouts/{rootKey}/metrics` returns `{catchingUp,payload}`. The payload carries
`metricsVersion,indexGeneration,indexRevision,scope,aggregate,role,selectedMetrics,declarations,files,directories`.
The optional paired pin is supplied as `indexGeneration` and `indexRevision`. File selectors require
`sourceSetId,language,path`; directory selectors require `sourceSetId,path` (empty path means the
source-set root); declaration selectors require `sourceSetId,path,sid` and no language. Source-set IDs
accept the emitted canonical `source-set:v1:path-<64 lowercase hex>` and
`source-set:v1:<lowercase hyphenated UUID>` forms. Never derive a source-set ID from the browser root key.

The optional `topCognitive=1..10` query adds `payload.topCognitive`; absent means this field is omitted.
It contains at most the requested count of callable rows from the selected scope, ordered by raw
`cognitive` descending, then byte ordering of `(sourceSetId,language,path,range.start,range.end,sid)`.
Each row has `sourceSetId,language,path,sid,range,name,kind,role,metrics`, where `name` is the native nullable
name, `kind` is the native callable kind, and `metrics` contains raw `cognitive,cyclomatic,maxNesting`.
Ranges remain half-open byte intervals. Anonymous names remain null, not invented labels. These three
raw scores are independent of `aggregate` and `selectedMetrics`; requesting `metrics=loc` does not
remove them. Empty measured scopes have an empty top list. Declaration scope ranks only its selected
callable. Existing declaration arrays and all fields when the option is absent remain unchanged.

Candidates are selected during the same pinned source computation and declaration fold, with only a
bounded top list retained, not additional per-file requests or a methods/name join. The existing
complete-envelope response bound, cancellation, deadline and final snapshot fence cover this list.
The role filter applies to the highest-cognitive list as well as declaration output and all
aggregates. A browser must not infer production status from filenames or treat the live filesystem
tree as indexed evidence.

## Hand-checked control-flow expectations

The `controls` fixtures intentionally cover mixed Boolean sequences and loops: Java `Controls.calc` cyclomatic/cognitive 7 (base, for, if, AND, OR, do, switch case); Python `controls.calc` cyclomatic/cognitive 7 (base, for, if, AND, OR, elif, while); JavaScript `controls.calc` cyclomatic/cognitive 6 (base, for-in, if, AND, OR, do); Rust `controls.calc` cyclomatic/cognitive 6 (base, for, if, AND, OR, while). Each `example` fixture has a non-default switch/match branch, a Boolean AND, and a conditional; Java/JavaScript have catch, Python has except, and Rust has no catch: expected cyclomatic Java 5, Python 5, JavaScript 5, Rust 4; expected cognitive Java/Python/JavaScript 4 and Rust 3 (if + Boolean sequence + switch/match, plus catch/except where present). `callback`/`test_callback` fixtures exercise test callbacks, anonymous lambdas/closures and named syntactic recursion; an anonymous callback does not create an Arborist function row. Hand-check named `calc` in each callback fixture as cyclomatic 2 (base + if); cognitive is 2 in Java/Python/JavaScript (if + syntactic self-call) and 3 in Rust (if + else + syntactic self-call). The **enclosing named test** rows in Java `testCalc`, Python `test_calc` and Rust `test_calc` each score cyclomatic 1 and cognitive 0: their separately represented callback bodies are excluded. Their separate native anonymous callable rows score Java 2/2, Python 2/2 and Rust 2/3 (cyclomatic/cognitive). JavaScript `test("calc", () => ...)` has no named enclosing test-function row: its outer anonymous callback scores 1/0, while its nested `callback = x => ...` scores 2/3; neither is an Arborist function row. Rust `?` is deliberately zero and implicit tail `n` is not a return. For `fn f() -> Result<i32,E> { let n = g()?; Ok(n) }`, hand-check cyclomatic = 1, cognitive = 0 and explicit exits = 0; the fallible call remains one call site. Check source changes against these counts rather than silently updating expected data.

## Oracle comparison and divergence register

The independent Arborist adapter is pinned to `StrangeDaysTech/arborist-metrics@d246d2e465346cbc2a7cc468d34ddbb7d77d5a0a` (four features only); RCA CLI is `mozilla/rust-code-analysis@57ab3973b53e7bf392d925cd5e6c6e889b8c22c0` / v0.0.25. See `tools/metrics-oracle/README.md` for exact locked manual invocations. CI reads committed JSON only and never downloads oracle dependencies. Correlate Arborist's *named* `FunctionMetrics` by exact `(fixture path,language,name,1-based inclusive start_line,end_line)` with native named declarations. A unique native candidate and unique oracle candidate are mandatory. Anonymous/lambdas, missing names, mismatched line boundaries and ambiguous tuples get `oracleUnavailable` reason `anonymous`, `missing`, `lineMismatch`, `ambiguous` respectively; they are not agreement. Arborist emits `cyclomatic,cognitive,sloc` per function. Its file-wide metrics are not declaration aggregates. RCA contributes only emitted comparable path/metric/granularity records; function-level RCA numbers require unambiguous emitted name/range. Never compare file-wide RCA metrics against declaration sums.

Finite fixture register: `tests/fixtures/metrics/golden/comparisons.json` covers 18 real sources (12 four-grammar control/example/callback fixtures, two JavaScript extraction fixtures, and four medium-cohort templates). Its 664 row associations contain 644 uniquely correlated native/oracle rows, 15 explicit anonymous unavailability entries (including six native anonymous callable rows), three genuinely missing named rows, and two RCA nested-space `granularityMismatch` rows. The 6,426 compared numeric cells contain 2,979 agreements and 3,447 recorded rule divergences, **each with the measured native/oracle values and its rule justification**. These are not claims that the distinct complexity/lexing algorithms agree generally. They cover every emitted Arborist cyclomatic/cognitive/SLOC field on uniquely matched rows, and every available RCA function-space cyclomatic/cognitive/SLOC, physical/comment lines, Halstead n1/n2/N1/N2/vocabulary/length/volume/difficulty/effort/bugs, nargs and nexits field on uniquely matched leaf spaces. RCA file-level metrics, MI variants, and nested-space sums are not substituted for a declaration result. Missing RCA functions and anonymous callbacks are unavailable, not zero or equality.

The golden `(sourceSetId,path,sid)` uses the fixed fixture-only source set `source-set:v1:oracle-fixtures`. In the offline test, actual native extraction runs over a fresh admitted copy of these sources; the test first proves each emitted native `syntax_id` is the exact `IdentityRegistry::stable` digest of that captured source set, path, language, ancestor keys and declaration key. It then applies the **same native identity algorithm** to the stable fixture-only source-set label and verifies every committed sid and every selected metric against the actual native declaration and `metrics::compute` result. The fixture-only label is not a runtime index source set and is never sent to Store. Its purpose is to make the committed golden independent of the temporary checkout path without inventing a declaration or oracle byte span. Tests also require every actual emitted oracle function/space to appear exactly once in the register and reject any comparable emitted field left unregistered.

Reason categories are finite: `anonymous` (no stable name tuple), `missing` (oracle name absent from native named callables), `lineMismatch` (name exists but physical-line boundaries differ), `ambiguous` (duplicate exact name+line candidates), and `granularityMismatch` (RCA sum includes nested spaces). The last two correlation boundary cases have dedicated synthetic negative tests, not false fixture claims. Every differing comparable cell has `ruleDivergence` and a stated explanation: branch grammar policies (switch/match heading versus arm and Boolean operators), cognitive depth/sequence/recursive-call policies, SLOC lexical/comment boundaries, Halstead token/body boundaries, explicit formal counts or implicit oracle exit counts. Rust `?` is zero here regardless of oracle policy. A missing or unbuilt oracle artifact cannot be fabricated as a measurement.

## File area metadata

Each per-file row also carries `physicalLines` (captured text's `str::lines().count()`)
and `fileRole` (the stored synthetic root-module role), independent of metric role or
aggregate. These come from the same indexed document as the callable metrics. Physical
lines include blank/comment lines and inline test lines; zero is valid for an empty
file. They are not summed callable LOC or physical production-only LOC. `roleCounts`
keeps its original callable-population meaning. See [Treemap](treemap.md).
