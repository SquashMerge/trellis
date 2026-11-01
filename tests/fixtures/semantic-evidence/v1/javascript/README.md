# JavaScript conformance corpus — native-v13 DRAFT

An independently authored event-processing workbench for #28: ten ES modules
cover parsing, normalization, stages, editing, history and runtime extensions.

**This is not the delivered semantic corpus.** `profile:"draft"` skips only
corpus count floors. All schema, source, measurement, coverage, identity and
publication checks still apply. Semantic captures, independently authored
scenarios and rooted answers await #88/#89. The 40-reference,
20-callable-value-negative and four imports/aliases floors are **not certified**;
Decision 0017 Amendments 3–5 retain those requirements for the final corpus.
JavaScript's unsupported-document floor is **0** under Amendment 4. No disabled
family, dropped record or artificial tagged-template gap is used to meet a floor.

## Current evidence

- 195 declarations, 196 invocations, 35 control regions and 1,045 native reference candidates.
- 10 production capability reports; every family is available with no gaps.
- Two independent command runs are byte-identical. Semantic counts remain zero.

The raw capture comes from Trellis's actual `trellis.native.syntax@native-v13`
extractor through the #95 `native-capture` command. It uses UTF-8 byte coordinates,
not the old helper's UTF-16 offsets. Native identifier-site candidates carry no
inferred semantic role or target. Narrow explicit renamed imports have measured
alias declarations; that alone does not prove a semantic alias.

Every document/revision has the production report for all four measurement
families. Fixture coverage intents mirror those reports exactly. All four families
are available with no gaps in this snapshot, including the call-free `public.js`
barrel. Zero calls is not unsupported. Annotations contain native coverage only;
scenario arrays, semantic artifacts, dispositions, callable negatives and rooted
answers are empty. `generated/manifest.json` selects the content-addressed bundle.

## Current native-v13 recapture

The #564 slice (a document past its stored evidence budgets keeps only its file
module and a coverage gap) re-ran the production `native-capture` command twice on
these unchanged snapshots using pinned Rust 1.99.1 and `test-support`. Both outputs
were byte-identical (SHA-256
`5200774bbed03e6124abe34d0730e1ea3ad946a9ffb0777c8fde955fec5c715f`). The compiled native extractor hash is
`ad5cf3093f15759ede50c5f1922e21d1b139d527482e2aba0638f7ef6e6ee195`, pinned as
`trellis-native-v13-javascript-test-support` in the hardware profile.

JavaScript measurement is unchanged: only the producer version, extractor hash and
occurrence IDs (which include the producer version) differ from the native-v12
capture, and the scenario table below uses the new occurrence refs.

## Historical native-v11 recapture

The #211 capture slice re-ran the production `native-capture` command twice on
these unchanged snapshots using pinned Rust 1.99.1 and `test-support`.
Both outputs were byte-identical (SHA-256 `2a580b1c45273f9fc4bc8983a25d5bcbc07257776ebce362643030f9637e6bf7`).
The compiled native extractor hash is `7bc7509763073366908a8a85758817984f7115a97f5a06f9e7f4b088923ee37b`,
pinned as `trellis-native-v11-javascript-test-support` in the hardware profile.
The descriptor and frozen source hashes are retained under
`~/projects/odin/trellis-evidence/issue-211-capture/`.

Declarations, source coordinates, headers, witnesses and capability reports are
unchanged. Calls, controls and reference candidates have new occurrence IDs
because occurrence identity includes the producer version. All other raw fields
are identical after that exact ID substitution. The scenario table below uses
the newly measured anchor refs; no authored answers, annotations, source,
scenarios or count floors changed. The draft remains incomplete.

## Historical native-v10 capture provenance


The hardware profile `docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json`
pins `trellis-native-v10-javascript-test-support`. It records the compiled
`TRELLIS_EXTRACTOR_HASH`, not the release binary hash or an archive digest.

| Pin | SHA-256 |
|---|---|
| native-v10 test-support extractor | `20da77888a20fd4fcb0e0bcdbd5ef067dbdc7384f56d6be86248196164655230` |
| Rust 1.99.1 compiler executable | `1ba7bb8e273bd64125f4048d2ecce530e0a6470e565ab1b3aecf0b82b532f7b1` |

The extractor combines main `700584be1e71f29b0e6e5dfb48487b47fdf88bb8` and feature-scip
`a84dd7d408e260c8c6260534ded73e8eafa6b838` in the native-v10 semantic sync. It preserves
native-v9 measurement and adds the whitespace module-range repair. Build with
pinned Rust 1.99.1 and `test-support`. Two fresh captures were byte-identical
(SHA-256 `f0219d7643de269f2649d42cb434d1df85340eee1b71d4eabb52ac8de7002bf2`).
Build inputs, descriptor, source hashes and both captures are retained under
`~/projects/odin/trellis-evidence/main-feature-scip-sync/`.
The hash includes the capture adapter and feature flag; it is not a release pin.
No executable, parser archive or fixture-local replacement extractor is vendored.
The obsolete TypeScript capture script and script-plus-parser archive pins are
removed. F7's Python/tarfile reproducibility dependency no longer exists.

## Reproduce the current native-v13 capture

The commands below reproduce the current capture, not the historical native-v10
or native-v11 hashes above. From the repository root, use an owned target directory and the
shared verification lock; do not run during another worker's measurements:

```sh
export CARGO_TARGET_DIR="$HOME/projects/odin/trellis-targets/javascript-recapture"
/opt/homebrew/bin/gtimeout -k 10s 1800s /usr/bin/lockf -k /tmp/trellis-factory-verify.lock cargo build --locked --features test-support --bin trellis
"$CARGO_TARGET_DIR/debug/trellis" native-capture --describe
/opt/homebrew/bin/gtimeout -k 10s 60s /usr/bin/lockf -k /tmp/trellis-factory-verify.lock "$CARGO_TARGET_DIR/debug/trellis" native-capture --fixture "$PWD/tests/fixtures/semantic-evidence/v1/javascript/fixture.json" --output /tmp/javascript-native-recapture.json
cmp /tmp/javascript-native-recapture.json tests/fixtures/semantic-evidence/v1/javascript/captures/native.json
node tools/semantic-contract/cli.mjs check --fixture javascript
node tools/semantic-contract/test/run.mjs
```

Use a fresh output path: capture refuses overwrite. A changed extractor hash
requires a re-pin and recapture. The command reads registered snapshot bytes,
not expected answers or semantic artifacts, and never runs repository code.
Native extraction inputs remain `[]`. The ES2022/ESNext/Bundler `allowJs` config
and dependency-free package describe the future semantic project, not native
inputs. The revision pins the Rust compiler executable, config and package bytes.

## Future semantic scenario inventory — not counted yet

Each row cites its independent source cause and a captured native anchor in
`captures/native.json`. These anchors prove syntax/coordinates only. The final
column states the semantic check still needed, **not an observed outcome**.
Lines describe frozen revision `r1`. Export aliases use identifier-site refs where production has no declaration anchor; no helper-only anchors are retained. Same-name cases use real getter/setter pairs,
not fabricated JavaScript overloads. Scoped shadowing also remains in the project.

| ID | Category | Owner | Source cause | Native anchor ref | Deferred semantic check |
|---|---|---|---|---|---|
| N1 | same-name/overload | EventEditor.key getter/setter | `src/editor.js:8` — `get key()` | `2:r1:sid:v1:92eb7a2fbb4fd6e81ac54b41a0515b28` | EventEditor.key getter/setter are real same-name, same-kind sibling methods under one class. Reading shows the committed key; assigning canonicalizes it. editSelection uses the setter, not an invented overload. |
| N2 | same-name/overload | EventEditor.payload getter/setter | `src/editor.js:10` — `get payload()` | `2:r1:sid:v1:302f9d6c22b6ef053921d300360d67d2` | EventEditor.payload getter/setter share one class owner. Reads retain the event payload; writes normalize whitespace through clean. The property contract provides natural sibling declarations. |
| N3 | same-name/overload | EventEditor.label getter/setter | `src/editor.js:12` — `get label()` | `2:r1:sid:v1:94de098569fe8c3ad8247f8717e72c93` | EventEditor.label getter/setter share one class owner. Reads fall back to the event key; writes clean and Unicode-normalize a display label. Distinct accessor bodies must retain their source identities. |
| N4 | same-name/overload | EventEditor.tags getter/setter | `src/editor.js:14` — `get tags()` | `2:r1:sid:v1:6e9398d62a78b31a178f4e7400a6ed2c` | EventEditor.tags getter/setter share one class owner. Reads return a copy; writes canonicalize and deduplicate tags. These protect editable collection state rather than padding same-name declarations. |
| I1 | imports/aliases | inspectBatch | `src/workbench.js:19` — `summary: summarize(events)` | `2:r1:occ:v2:ddb73acd44cf9e7f54dcdf93e1266ade` | The default import summarize is called by inspectBatch; producer binding must resolve through the default export in text.js. |
| I2 | imports/aliases | exportSelection → map callback | `src/workbench.js:48` — `key: keyOf(event.key)` | `2:r1:occ:v2:41ca2d7d8d810af63e6445fa929e873b` | keyOf imports the exported eventKey alias of canonicalKey, while serialize aliases formatLine. Both are real source aliases, not renamed normalized facts. |
| I3 | imports/aliases | runWorkbench | `src/workbench.js:36` — `const output = sendBatch(quiet, events);` | `2:r1:occ:v2:7a4a564dc7383a4095305557b338c575` | sendBatch is imported from public.js, which re-exports adapters.deliver. The barrel contains no wrapper call to confuse with the target. |
| I4 | imports/aliases | compareLabels | `src/workbench.js:53` — `text.describe(event)];` | `2:r1:occ:v2:547f469099b86e838ac16ed00c9cda00` | Namespace import text selects describe independently of compareLabels nested same-name function. Exact callee join must preserve the qualification. |
| V1 | callable values | toolPalette | `src/pipeline.js:7` — `clean, canonicalKey, parseLine, hasPayload, stamp, redact, formatLine, describe, café, 𐐀label,` | `2:r1:occ:v2:917a5cb9e456099fe38581ea9eb89a97` | toolPalette retains ten helper function values in object shorthand properties. None of these reads invokes the helpers. |
| V2 | callable values | historyActions | `src/history.js:55` — `return { flatten, checksum, replayEvent, replayCheckpoint, findBatch,` | `2:r1:occ:v2:72aebb87f86b043b2936115418f4d8b8` | historyActions exposes recursive helpers as values. Merely opening action metadata must not traverse the history or create recursion edges. |
| V3 | callable values | previewTools | `src/workbench.js:43` — `return { ...palette, ...actions, prepare, inspectBatch, captions, runWorkbench,` | `2:r1:occ:v2:ab33b83a60fc1a34cd2c12a0e0190af8` | previewTools returns workbench functions and more helpers as values. The toolPalette/historyActions calls occur before these reads and are distinct invocations. |
| V4 | callable values | adapterMethods | `src/legacy.js:32` — `return { append: source.append, drain: source.drain };` | `2:r1:occ:v2:aeeb87c71f16ae21e55128e85c41fb49` | adapterMethods reads prototype-provided callable properties. Read syntax alone must not create append or drain execution; binding may remain dynamic. |
| R1 | recursion | checksum | `src/history.js:11` — `return text.charCodeAt(index) + checksum(text, index + 1);` | `2:r1:occ:v2:4d6bfead0f7caf525997a733c32c81cb` | checksum has one direct self-call behind a terminating base case. The external charCodeAt call is a distinct earlier sibling. |
| R2 | recursion | History.lookup | `src/history.js:47` — `return local ?? this.parent?.lookup(key);` | `2:r1:occ:v2:4ea50f3ffac75feaccb08b58eb600e06` | History.lookup walks the parent chain through its own method. Future counting needs an exact internal self-target on this optional call; the callback-only flatten reference is not a direct self-call. |
| R3 | recursion | replayCheckpoint | `src/history.js:22` — `return cursor.checkpoint ? replayEvent(cursor.next) : replayCheckpoint(cursor.next);` | `2:r1:occ:v2:17cbb3ddcb9d3552255e4da7e4bfe57b` | replayCheckpoint has a direct self-call and a separate mutual edge to replayEvent. Only exact self-target proof establishes this recursion scenario; future rooted answers must retain cycle boundaries. |
| R4 | recursion | findBatch → visit | `src/history.js:29` — `const found = visit(child);` | `2:r1:occ:v2:d10ec5fc3dcab4d5720632a5fc98c0fb` | findBatch owns a nested recursive visit. Its recursive call and initial visit(root) have the nested owner target, not the outer findBatch symbol. |
| D1 | relationships/dispatch | module (ParseStage declaration) | `src/stages.js:12` — `export class ParseStage extends Stage` | `2:r1:sid:v1:951c7e24ad1e2928d600c6211c31a26f` | ParseStage has an explicit extends edge. Four other functional stages share the base for real parse/filter/stamp/redact/format operations. |
| D2 | relationships/dispatch | StampStage.transform | `src/stages.js:26` — `transform(value) { return stamp(value); }` | `2:r1:sid:v1:107a78c7787a15353a66e41d4197f0bb` | StampStage overrides Stage.transform. Each stage overrides accepts/transform/describe; future relationships require independent emitted relationship records. |
| D3 | relationships/dispatch | planRetry | `src/adapters.js:73` — `wait: policy.delay(attempt),` | `2:r1:occ:v2:3faa94c0f8791a8fa2a44582c793bebd` | planRetry picks ImmediatePolicy or RetryPolicy and calls delay on that union. Two real declarations are possible; ambiguity requires observed conflicting exact producer targets. |
| D4 | relationships/dispatch | Pipeline.run | `src/pipeline.js:21` — `value = stage.run(value);` | `2:r1:occ:v2:f3edd80eb9cd91c977e0e3a3ba30eec0` | Pipeline executes stages through inherited Stage.run. Calls to this.accepts/this.transform inside the base must not automatically promote subclass possibilities to direct execution. |
| U1 | Unicode/coordinates | module (café declaration) | `src/text.js:36` — `export function café(event)` | `2:r1:sid:v1:17e2f06c2a644eac1ccc70c8a217a2e0` | The precomposed accented function name occupies different UTF-8-byte and UTF-16-column widths; exact declaration-name witnesses must preserve spelling. |
| U2 | Unicode/coordinates | module (𐐀label declaration) | `src/text.js:40` — `export function 𐐀label(event)` | `2:r1:sid:v1:67a313e3952c7a74d18d5870a91cbe56` | The astral letter is a valid identifier at ES2022. Its name range spans a surrogate pair in the SCIP producer encoding. |
| U3 | Unicode/coordinates | captions | `src/workbench.js:24` — `const heading = 𐐀label(event);` | `2:r1:occ:v2:9f7fc246d301236602c1e02b2d8ce89f` | An imported astral identifier is called, distinct from its declaration. Exact callee and invocation ranges must map through UTF-16 correctly. |
| U4 | Unicode/coordinates | captions | `src/workbench.js:25` — `const emoji = "📨"; return text.describe(event)` | `2:r1:occ:v2:11b95820019af04ec80d5e1ef0e2a397` | An astral string literal precedes a namespace call on the same physical line; byte and UTF-16 columns diverge before that callee. |
| F1 | coverage/freshness | receive | `src/plugins.js:10` — `const decoded = extension.decode(message);` | `2:r1:occ:v2:59e9948d233b295c8b1faf11586332f7` | A valid measured invocation on an untyped runtime extension. Future unresolved disposition needs a complete producer tuple plus no exact occurrence; absence alone in an omitted tuple proves nothing. |
| F2 | coverage/freshness | stamp | `src/text.js:20` — `return { ...event, observedAt: Date.now() };` | `2:r1:occ:v2:582ae636fa9a16ed7278b5f07ea99022` | Small isolated stamp body is a future changed-bytes case: replacing only this call must invalidate old document content/occurrence evidence. No second revision or stale fact is claimed now. |
| F3 | coverage/freshness | Pipeline.constructor | `src/pipeline.js:12` — `constructor(stages = defaultStages())` | `2:r1:sid:v1:6d9e73cd12f32529ea92143d9b4ac3b3` | Native extraction declares extractionInputs:[] and does not read project configuration. A future semantic config-only mutation must invalidate the semantic basis/config provenance, not claim a native extraction-context change. |
| F4 | coverage/freshness | module (export-clause identifier) | `src/public.js:3` — `export { Pipeline as EventPipeline }` | `2:r1:occ:v2:0bfdbd2deb2ec82af3191a14b7f66008` | The re-export alias EventPipeline has a measured identifier site, not an invented alias declaration. The barrel has no calls and no capability gaps; future coverage checks must preserve that distinction rather than manufacture unsupported. |
| C1 | compatibility/control | Pipeline.run | `src/pipeline.js:20` — `if (value === undefined) break;` | `2:r1:occ:v2:cb3856b8abda74615e0e1a592024bb46` | Pipeline.run has a loop and early break. stage.run is inside the loop and after the guard; controls and order remain native facts. |
| C2 | compatibility/control | withSession | `src/plugins.js:53` — `await consume(event);` | `2:r1:occ:v2:ff600f02f3574df3d7f163938a1ec331` | withSession has awaited callback execution inside a loop under try, with catch host.report and finally disconnect. Callback bodies must not be folded into caller ownership. |
| C3 | compatibility/control | schedulePreview → then callback (nested map callback) | `src/pipeline.js:54` — `const events = batch.map((line) => pipeline.run(line));` | `2:r1:occ:v2:eca0cf807b4b44c1b950575aecc10934` | schedulePreview nests a map callback inside a Promise.then callback. The pipeline.run call belongs to the inner callback boundary, not schedulePreview directly. |
| C4 | compatibility/control | module (assigned function expression) | `src/legacy.js:22` — `LegacySource.prototype.append.call(this,` | `2:r1:occ:v2:84905acd94e8cdc761a0e5e5ed49a385` | TaggedSource uses prototype assignment and explicit .call delegation. Production attributes this call to the module; it does not emit a separate anonymous-function declaration for the assigned function expression. Preserve that measured owner. Prototype metadata is not a fabricated ES extends relationship and .call syntax is not silently rewritten to direct execution. |

## Deferred floor plan and producer quirks


- **120 measured calls / 40 semantic references:** the source has real parsing, normalization, arrays, serialization, classes, history, host calls and control/callback bodies. Totals must come from native records. No source regex count, design estimate or declared target is an observed floor. If native measurement misses syntax, document the gap rather than adding duplicate calls.
- **20 resolved:** direct helpers, imported functions, recursion and constructors are natural candidate anchors; only exact admitted producer joins count.
- **20 proven-external:** text operations (`String`, `trim`, `toLowerCase`, `replaceAll`, `JSON.parse`, `JSON.stringify`, `Date.now`, `normalize`, `toUpperCase`, map/join), adapter arrays/logging and history `Map`/`Array` methods provide candidates. Built-in namespace merging or absence may change outcomes; do not classify by spelling alone. A local small .d.ts dependency is an optional future owner decision, not required or authored here.
- **20 ambiguous:** adapters.roundTrip has five conditional codec calls; deliver has five sink calls; planRetry has five policy calls; stages.preview has three union-stage calls and explainIngress has two. These are **20 candidate sites**, not 20 observed conflicts. Only two or more distinct producer targets at the same exact native join may establish ambiguity. If producer inference collapses any union, report the shortfall rather than inventing a second target.
- **20 unresolved:** plugins.js has independent host/session/extension/registry operations (connect/authenticate/subscribe, decode/validate/enrich/acknowledge, encode/send/checkpoint, flush/unsubscribe/close, register/create/initialize, lookup, dispose/unregister, poll/report). There are 21 ordinary member-call candidates, a one-site design margin, not 21 observed unresolved outcomes. `factory` is passed as a value to `registry.register`; installation now uses `registry.create(name, options)` rather than calling the parameter. `consume(event)` is a direct parameter-bound callback call and the computed `plugin[operation](event)` has no measured callee range; neither belongs in this call-callee budget. Distinct protocol operations are useful behavior, not repeated padding. A complete producer tuple and exact missing occurrence are required by Decision 0017 Amendment 1. Merely untyped-looking syntax is not an outcome.
- **20 callable-value negatives:** toolPalette has ten helper reads; historyActions has five direct helper reads; previewTools retains eight workbench/history helpers as values. Those 23 direct helper reads are source candidates; member reads in historyActions/adapterMethods add useful dynamic cases but are not needed to assert a design total. Only exact internal callable targets, read without call and outside callee ranges count. Shorthand support and producer joins must be observed, not assumed.
- **20 relationships:** five functional Stage subclasses each have an explicit extends and three overriding methods (accepts/transform/describe), giving 20 source relationship candidates. Real producer relationship records must establish them. Prototype assignment in legacy.js is deliberately not counted as a class extends edge.
- **Required roles:** declarations, helper values, state writes (`processed`, history entries and sink buffers), calls, ES imports, alias names, `new` and `extends` supply syntax candidates. Applicable-role counts must come from admitted native and producer records.
- **Unsupported documents (F3):** the JavaScript floor is 0. All ten production reports have no gaps; intents mirror them. `public.js` remains a measured call-free barrel, not an unsupported fixture. Tagged templates are already measured; do not invent a gap or disable a working family.
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

## Review follow-up

- **F3:** production capability reports replace authored conformance authority;
  their exact mirror is checked. No unsupported document is manufactured (floor 0).
- **F6:** installation uses the registered-plugin creation operation, removing the
  direct `factory` call from the candidate budget. Counts remain native-only;
  absence-based semantic outcomes await complete real producer tuples and exact joins.
- **F7:** delete the obsolete script/archive and pin the compiled native capture
  build and Rust compiler instead. No Python archive writer is needed.

Historical native-v10 capture, descriptor, hash-binding and review evidence is under
`~/projects/odin/trellis-evidence/main-feature-scip-sync/`. Historical native-v9
evidence remains under `~/projects/odin/trellis-evidence/issue-28/native-v9/`;
the older watcher failure remains in its parent directory. Neither is erased or
retried unchanged. This sync does not complete #28 or certify semantic floors.
