# JavaScript conformance corpus — native-only DRAFT

An independently authored event-processing workbench for #28. It covers parsing,
normalization, stage dispatch, editing, delivery, history, runtime extensions and
prototype behavior across ten ES modules.

**This is not the delivered semantic corpus.** Decision 0017 §3 permits this
native-only draft. Semantic captures and outcome answers wait for #87–#90 and
#62. There are no semantic artifacts, captured semantic producers, semantic
facts, rooted answers, disposition assertions or callable-value negatives.
Annotation `scenarios` arrays are empty: the checker requires real semantic proof
even for draft scenario records. The inventory below is a future scenario plan,
not 32 validated scenarios or a promise that a producer will emit particular facts.

## Current evidence

- `fixture.json`: `language: "javascript"`, `profile: "draft"`, source set `main`, revision `r1`.
- `captures/native.json`: 258 declarations, 196 invocations,
  34 control regions and 681 candidate reference tokens measured from ASTs.
- Generated counts report 196 measured calls; all semantic counts and observed semantic roles are zero.
  Candidate native references are not installed semantic References.
- Adjacent annotations record native coverage only. All four measurement families
  are available. `public.js` requests/observes definition only because it has no
  invocations; the other documents request/observe definition and call.
- Draft skips corpus floors only. Schema, source witnesses, ranges, IDs, owners,
  control containment, coverage, provenance, publication and determinism still run.
  `generated/manifest.json` names the active content-addressed bundle; do not edit it.

## Capture provenance and reproduction

`capture-native.mjs` is a **fixture-local conformance `native@1` producer**, not
Trellis's production `trellis.native.syntax` extractor. It uses the frozen
TypeScript 5.9.3 parser's JavaScript AST, not a compiler program or type checker.
It reads source bytes/path/language only (`extractionInputs: []`), resolves no
imports and reads no tsconfig or package file. The explicit ES2022/ESNext/Bundler
`allowJs` configuration and dependency-free ES-module package describe the future
semantic project; they do not influence these native measurements.

All input files are registered in `fixture.json`. The hardware profile
`docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json` authenticates:

| Artifact | SHA-256 |
|---|---|
| Node 24.11.1 darwin-arm64 executable | `4255a388254ca4319e2f95f1da375d5deaddf25baf9c7c85070b67f9543b15d0` |
| TypeScript 5.9.3 `lib/typescript.js` | `3ae902c92cc44dace175c0e69e13a4b0899f6983c6121d76b9ab8dd5795e7675` |
| This capture script | `0ff517d5e639f287241a3ee5f8a660ca079ad2171c5b19e0739de558ab741832` |
| Native producer archive (script + parser) | `9576f1f3ec87b33d49cc0355ce898f83a815c662eeab185b566a5df0206bb2e0` |

The producer archive lives outside the repository; no executable binary or parser
bundle is vendored. Its reproducible format is uncompressed Python `tarfile`
`USTAR_FORMAT`, exactly two members in order: `capture-native.mjs`, `typescript.js`.
Both have mode `0644`, mtime/uid/gid zero and empty uname/gname. Member bytes are
exactly the files identified above. This pins the parser too, not just a script
that could load an arbitrary implementation. To rebuild the archive, set
`PARSER` to the verified absolute parser path, then run from the repository root:

```sh
python3 - "$PARSER" /tmp/javascript-native-1.tar <<'PY'
import io, pathlib, sys, tarfile
fixture = pathlib.Path("tests/fixtures/semantic-evidence/v1/javascript")
with tarfile.open(sys.argv[2], "w", format=tarfile.USTAR_FORMAT) as archive:
    for name, path in [("capture-native.mjs", fixture / "capture-native.mjs"),
                       ("typescript.js", pathlib.Path(sys.argv[1]))]:
        data = path.read_bytes()
        info = tarfile.TarInfo(name)
        info.size, info.mode = len(data), 0o644
        info.mtime = info.uid = info.gid = 0
        info.uname = info.gname = ""
        archive.addfile(info, io.BytesIO(data))
PY
shasum -a 256 /tmp/javascript-native-1.tar
```

Verify Node, script and parser hashes before capture. Use a fresh output path;
the capture refuses overwrite. It can run from any working directory:

```sh
FIXTURE="$PWD/tests/fixtures/semantic-evidence/v1/javascript"
node "$FIXTURE/capture-native.mjs" --typescript "$PARSER" \
  --snapshot-root "$FIXTURE/snapshots/r1" --source-set main --revision r1 \
  --output /tmp/javascript-native-recapture.json
cmp /tmp/javascript-native-recapture.json "$FIXTURE/captures/native.json"
node tools/semantic-contract/cli.mjs check --fixture javascript
node tools/semantic-contract/test/run.mjs
```

The authoring producer measures actual functions, arrows, classes, accessors,
constructors, simple variables and import/export alias bindings. Calls include
`CallExpression`/`NewExpression`; computed callee spellings remain null. Controls
and occurrences keep nearest callable/module ownership across callbacks. Text
leaves have exact source witnesses. Node offsets are UTF-16 code units, including
astral identifiers and text before same-line calls. There are no inferred targets,
semantic roles, symbols, relationship facts or direct-execution answers.

Limits: no JSX, decorators, anonymous classes, typed syntax, destructured
parameters, non-identifier declaration names, synthesized constructors or implicit
calls. Fields/parameters/destructured bindings have no separate declaration rows;
tagged templates have no invocation rows. Plain assignment write-only targets are
not candidate references. No claim of a general production JavaScript frontend is
made. The source avoids unsupported declaration forms.

## Future semantic scenario inventory — not counted yet

Each row cites its independent source cause and a captured native anchor in
`captures/native.json`. These anchors prove syntax/coordinates only. The final
column states the semantic check still needed, **not an observed outcome**.
Lines describe frozen revision `r1`. Same-name cases use real getter/setter pairs,
not fabricated JavaScript overloads. Scoped shadowing also remains in the project.

| ID | Category | Owner | Source cause | Native anchor ref | Deferred semantic check |
|---|---|---|---|---|---|
| N1 | same-name/overload | EventEditor.key getter/setter | `src/editor.js:8` — `get key()` | `r1:src/editor.js:declaration:GetAccessor:214:250` | EventEditor.key getter/setter are real same-name, same-kind sibling methods under one class. Reading shows the committed key; assigning canonicalizes it. editSelection uses the setter, not an invented overload. |
| N2 | same-name/overload | EventEditor.payload getter/setter | `src/editor.js:10` — `get payload()` | `r1:src/editor.js:declaration:GetAccessor:312:356` | EventEditor.payload getter/setter share one class owner. Reads retain the event payload; writes normalize whitespace through clean. The property contract provides natural sibling declarations. |
| N3 | same-name/overload | EventEditor.label getter/setter | `src/editor.js:12` — `get label()` | `r1:src/editor.js:declaration:GetAccessor:419:477` | EventEditor.label getter/setter share one class owner. Reads fall back to the event key; writes clean and Unicode-normalize a display label. Distinct accessor bodies must retain their source identities. |
| N4 | same-name/overload | EventEditor.tags getter/setter | `src/editor.js:14` — `get tags()` | `r1:src/editor.js:declaration:GetAccessor:553:604` | EventEditor.tags getter/setter share one class owner. Reads return a copy; writes canonicalize and deduplicate tags. These protect editable collection state rather than padding same-name declarations. |
| I1 | imports/aliases | inspectBatch | `src/workbench.js:19` — `summary: summarize(events)` | `r1:src/workbench.js:call:CallExpression:824:841` | The default import summarize is called by inspectBatch; producer binding must resolve through the default export in text.js. |
| I2 | imports/aliases | exportSelection → map callback | `src/workbench.js:48` — `key: keyOf(event.key)` | `r1:src/workbench.js:call:CallExpression:1886:1902` | keyOf imports the exported eventKey alias of canonicalKey, while serialize aliases formatLine. Both are real source aliases, not renamed normalized facts. |
| I3 | imports/aliases | runWorkbench | `src/workbench.js:36` — `const output = sendBatch(quiet, events);` | `r1:src/workbench.js:call:CallExpression:1422:1446` | sendBatch is imported from public.js, which re-exports adapters.deliver. The barrel contains no wrapper call to confuse with the target. |
| I4 | imports/aliases | compareLabels | `src/workbench.js:53` — `text.describe(event)];` | `r1:src/workbench.js:call:CallExpression:2061:2081` | Namespace import text selects describe independently of compareLabels nested same-name function. Exact callee join must preserve the qualification. |
| V1 | callable values | toolPalette | `src/pipeline.js:7` — `clean, canonicalKey, parseLine, hasPayload, stamp, redact, formatLine, describe, café, 𐐀label,` | `r1:src/pipeline.js:reference:Identifier:300:305` | toolPalette retains ten helper function values in object shorthand properties. None of these reads invokes the helpers. |
| V2 | callable values | historyActions | `src/history.js:55` — `return { flatten, checksum, replayEvent, replayCheckpoint, findBatch,` | `r1:src/history.js:reference:Identifier:1515:1522` | historyActions exposes recursive helpers as values. Merely opening action metadata must not traverse the history or create recursion edges. |
| V3 | callable values | previewTools | `src/workbench.js:43` — `return { ...palette, ...actions, prepare, inspectBatch, captions, runWorkbench,` | `r1:src/workbench.js:reference:Identifier:1692:1699` | previewTools returns workbench functions and more helpers as values. The toolPalette/historyActions calls occur before these reads and are distinct invocations. |
| V4 | callable values | adapterMethods | `src/legacy.js:32` — `return { append: source.append, drain: source.drain };` | `r1:src/legacy.js:reference:Identifier:942:948` | adapterMethods reads prototype-provided callable properties. Read syntax alone must not create append or drain execution; binding may remain dynamic. |
| R1 | recursion | checksum | `src/history.js:11` — `return text.charCodeAt(index) + checksum(text, index + 1);` | `r1:src/history.js:call:CallExpression:377:402` | checksum has one direct self-call behind a terminating base case. The external charCodeAt call is a distinct earlier sibling. |
| R2 | recursion | History.lookup | `src/history.js:47` — `return local ?? this.parent?.lookup(key);` | `r1:src/history.js:call:CallExpression:1339:1363` | History.lookup walks the parent chain through its own method. Future counting needs an exact internal self-target on this optional call; the callback-only flatten reference is not a direct self-call. |
| R3 | recursion | replayCheckpoint | `src/history.js:22` — `return cursor.checkpoint ? replayEvent(cursor.next) : replayCheckpoint(cursor.next);` | `r1:src/history.js:call:CallExpression:734:763` | replayCheckpoint has a direct self-call and a separate mutual edge to replayEvent. Only exact self-target proof establishes this recursion scenario; future rooted answers must retain cycle boundaries. |
| R4 | recursion | findBatch → visit | `src/history.js:29` — `const found = visit(child);` | `r1:src/history.js:call:CallExpression:956:968` | findBatch owns a nested recursive visit. Its recursive call and initial visit(root) have the nested owner target, not the outer findBatch symbol. |
| D1 | relationships/dispatch | module (ParseStage declaration) | `src/stages.js:12` — `export class ParseStage extends Stage` | `r1:src/stages.js:declaration:ClassDeclaration:312:529` | ParseStage has an explicit extends edge. Four other functional stages share the base for real parse/filter/stamp/redact/format operations. |
| D2 | relationships/dispatch | StampStage.transform | `src/stages.js:26` — `transform(value) { return stamp(value); }` | `r1:src/stages.js:declaration:MethodDeclaration:879:920` | StampStage overrides Stage.transform. Each stage overrides accepts/transform/describe; future relationships require independent emitted relationship records. |
| D3 | relationships/dispatch | planRetry | `src/adapters.js:73` — `wait: policy.delay(attempt),` | `r1:src/adapters.js:call:CallExpression:2453:2474` | planRetry picks ImmediatePolicy or RetryPolicy and calls delay on that union. Two real declarations are possible; ambiguity requires observed conflicting exact producer targets. |
| D4 | relationships/dispatch | Pipeline.run | `src/pipeline.js:21` — `value = stage.run(value);` | `r1:src/pipeline.js:call:CallExpression:654:670` | Pipeline executes stages through inherited Stage.run. Calls to this.accepts/this.transform inside the base must not automatically promote subclass possibilities to direct execution. |
| U1 | Unicode/coordinates | module (café declaration) | `src/text.js:36` — `export function café(event)` | `r1:src/text.js:declaration:FunctionDeclaration:827:917` | The precomposed accented function name occupies different UTF-8-byte and UTF-16-column widths; exact declaration-name witnesses must preserve spelling. |
| U2 | Unicode/coordinates | module (𐐀label declaration) | `src/text.js:40` — `export function 𐐀label(event)` | `r1:src/text.js:declaration:FunctionDeclaration:919:989` | The astral letter is a valid identifier at ES2022. Its name range spans a surrogate pair in the SCIP producer encoding. |
| U3 | Unicode/coordinates | captions | `src/workbench.js:24` — `const heading = 𐐀label(event);` | `r1:src/workbench.js:call:CallExpression:959:973` | An imported astral identifier is called, distinct from its declaration. Exact callee and invocation ranges must map through UTF-16 correctly. |
| U4 | Unicode/coordinates | captions | `src/workbench.js:25` — `const emoji = "📨"; return text.describe(event)` | `r1:src/workbench.js:call:CallExpression:1004:1024` | An astral string literal precedes a namespace call on the same physical line; byte and UTF-16 columns diverge before that callee. |
| F1 | coverage/freshness | receive | `src/plugins.js:10` — `const decoded = extension.decode(message);` | `r1:src/plugins.js:call:CallExpression:357:382` | A valid measured invocation on an untyped runtime extension. Future unresolved disposition needs a complete producer tuple plus no exact occurrence; absence alone in an omitted tuple proves nothing. |
| F2 | coverage/freshness | stamp | `src/text.js:20` — `return { ...event, observedAt: Date.now() };` | `r1:src/text.js:call:CallExpression:538:548` | Small isolated stamp body is a future changed-bytes case: replacing only this call must invalidate old document content/occurrence evidence. No second revision or stale fact is claimed now. |
| F3 | coverage/freshness | Pipeline.constructor | `src/pipeline.js:12` — `constructor(stages = defaultStages())` | `r1:src/pipeline.js:declaration:Constructor:430:523` | Native extraction declares extractionInputs:[] and does not read project configuration. A future semantic config-only mutation must invalidate the semantic basis/config provenance, not claim a native extraction-context change. |
| F4 | coverage/freshness | module (export clause) | `src/public.js:3` — `export { Pipeline as EventPipeline }` | `r1:src/public.js:declaration:Identifier:175:188` | A real executable-free barrel still has declaration/import/reference surface. Future coverage tests must distinguish unsupported native measurement family from zero calls; do not manufacture unsupported by dropping records. |
| C1 | compatibility/control | Pipeline.run | `src/pipeline.js:20` — `if (value === undefined) break;` | `r1:src/pipeline.js:control:IfStatement:608:639` | Pipeline.run has a loop and early break. stage.run is inside the loop and after the guard; controls and order remain native facts. |
| C2 | compatibility/control | withSession | `src/plugins.js:53` — `await consume(event);` | `r1:src/plugins.js:call:CallExpression:1507:1521` | withSession has awaited callback execution inside a loop under try, with catch host.report and finally disconnect. Callback bodies must not be folded into caller ownership. |
| C3 | compatibility/control | schedulePreview → then callback (nested map callback) | `src/pipeline.js:54` — `const events = batch.map((line) => pipeline.run(line));` | `r1:src/pipeline.js:call:CallExpression:1413:1431` | schedulePreview nests a map callback inside a Promise.then callback. The pipeline.run call belongs to the inner callback boundary, not schedulePreview directly. |
| C4 | compatibility/control | TaggedSource.prototype.append anonymous function | `src/legacy.js:22` — `LegacySource.prototype.append.call(this,` | `r1:src/legacy.js:call:CallExpression:636:701` | TaggedSource uses prototype assignment and explicit .call delegation. Prototype metadata is not a fabricated ES extends relationship and .call syntax is not silently rewritten to direct execution. |

## Deferred floor plan and producer quirks


- **120 measured calls / 40 references:** the source has real parsing, normalization, arrays, serialization, classes, history, host calls and control/callback bodies. The parent must obtain totals from native records. No source regex count, design estimate or declared target is an observed floor. If native measurement misses syntax, document the gap rather than adding duplicate calls.
- **20 resolved:** direct helpers, imported functions, recursion and constructors are natural candidate anchors; only exact admitted producer joins count.
- **20 proven-external:** text operations (`String`, `trim`, `toLowerCase`, `replaceAll`, `JSON.parse`, `JSON.stringify`, `Date.now`, `normalize`, `toUpperCase`, map/join), adapter arrays/logging and history `Map`/`Array` methods provide candidates. Built-in namespace merging or absence may change outcomes; do not classify by spelling alone. A local small .d.ts dependency is an optional future owner decision, not required or authored here.
- **20 ambiguous:** adapters.roundTrip has five conditional codec calls; deliver has five sink calls; planRetry has five policy calls; stages.preview has three union-stage calls and explainIngress has two. These are **20 candidate sites**, not 20 observed conflicts. Only two or more distinct producer targets at the same exact native join may establish ambiguity. If producer inference collapses any union, report the shortfall rather than inventing a second target.
- **20 unresolved:** plugins.js has independent host/session/extension/registry/factory operations (connect/authenticate/subscribe, decode/validate/enrich/acknowledge, encode/send/checkpoint, flush/unsubscribe/close, register/factory/initialize, lookup/computed operation, dispose/unregister, poll/consume/report). Distinct protocol operations are useful behavior, not repeated padding. A complete producer tuple and exact missing occurrence are required by Decision 0017 Amendment 1. Merely untyped-looking syntax is not an outcome.
- **20 callable-value negatives:** toolPalette has ten helper reads; historyActions has five direct helper reads; previewTools retains eight workbench/history helpers as values. Those 23 direct helper reads are source candidates; member reads in historyActions/adapterMethods add useful dynamic cases but are not needed to assert a design total. Only exact internal callable targets, read without call and outside callee ranges count. Shorthand support and producer joins must be observed, not assumed.
- **20 relationships:** five functional Stage subclasses each have an explicit extends and three overriding methods (accepts/transform/describe), giving 20 source relationship candidates. Real producer relationship records must establish them. Prototype assignment in legacy.js is deliberately not counted as a class extends edge.
- **Required roles:** declarations, helper values, state writes (`processed`, history entries and sink buffers), calls, ES imports, alias names, `new` and `extends` supply syntax candidates. Applicable-role counts must come from admitted native and producer records.
- **Unsupported document:** public.js is a natural executable-free barrel, but zero calls is not unsupported measurement. Select a document only if a real native family is unavailable with its actual diagnostic and producer facts. Do not turn off a supported family to hit the floor.
- **Freshness and determinism:** F2–F3 reserve concrete future mutation subjects; r1 alone proves neither revision rejection nor three-import determinism. Future checks must use measured bytes/configuration/provenance and retain source-order traversal, cycles, callback boundaries and control regions. Native extractionInputs is []: configuration freshness here concerns the future semantic basis only, not native extraction context.


No unsupported outcome is asserted: this native producer has all four supported
measurement families on these documents. Future unavailable-family coverage must
have a real diagnostic and authenticated non-bookkeeping semantic evidence in the
document; intentionally dropping records would not establish unsupported.

`scip-typescript` 0.4.0 is installed/pinned in the hardware profile but is **not** a
captured producer in this fixture. Its declaration files can supply external
targets; built-in merged globals may yield several targets at one exact range.
Union receiver expressions are ambiguity candidates, not guaranteed conflicts.
Future capture must retain those distinctions and use the trusted flat SCIP
manifest limits. Never use `-infer-tsconfig`: its ES5 defaults drop the astral
identifier used here. Preserve the explicit project configuration, dependency and
runtime hashes, exact joins and producer symbols separate from stable syntax IDs.

For the final corpus, author semantic annotations and rooted expected answers
independently from real captured records. Do not copy evaluator output into
answers, relabel dynamic/possible/external evidence as direct calls, or pad counts.
Only then may this fixture become `profile: "corpus"` and satisfy the full floors.
