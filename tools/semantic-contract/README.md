# Semantic-contract corpus tooling (v1)

Offline Node 24 ESM checking tooling. Normal checking requires no install, network, server API, or language parser. The optional production-native capture path below uses a pinned Trellis test-support build; it does not import or publish an index. The normative contract is `docs/semantic-evidence/contract-v1.md`; its #22 record shapes and enum orders are preserved. `schema.mjs` owns **all** closed record, authored input, answer, count and manifest shapes. `formats.mjs` checks their fields recursively; downstream modules must use these definitions rather than copy or relax them. Shape checks are distinct from cross-field/source/provenance/graph invariants.

## Commands

- `node tools/semantic-contract/test/run.mjs` runs the finite shared tests, mutations and discovered fixtures. `npm --prefix tools/semantic-contract test` is its offline alias.
- `node tools/semantic-contract/cli.mjs generate` validates inputs, independently authored answers and floors, then publishes verified immutable output.
- `node tools/semantic-contract/cli.mjs generate --check` checks byte-identical regeneration without writing fixtures.
- `node tools/semantic-contract/cli.mjs check` checks published bytes, input hashes and every invariant without writing fixtures.
- The CLI accepts `--fixture example` and `--fixtures-root <path>`; the runner accepts and forwards `--fixtures-root <path>`.
- `node tools/semantic-contract/author-helper.mjs hashes [fixture-directory]` prints actual source/capture SHA-256 digests, source-manifest digests, and each captured file’s declared hash alongside the actual hash. `node tools/semantic-contract/author-helper.mjs ids [fixture-directory]` prints source-checked native declaration/occurrence IDs, ordinals, ancestor keys and header/sibling-group digests. With no path, both use `tests/fixtures/semantic-evidence/v1/example`. Neither command reads authored answers or writes any fixture/expected/generated file.

The common runner executes a fixed list of shared Node tests, discovers every immediate fixture directory, and checks generation, publication, and byte-identical regeneration in isolated temporary copies. `--fixtures-root` selects an alternate v1 directory for both shared discovery tests and the runner. The focused fixture gate is `node --test tools/semantic-contract/test/fixture.test.mjs`. There is no network dependency. A failed fixture exits nonzero with the fixture, tooling assertion ID, #22 error class where applicable, field path, and concrete message. Other valid published manifests remain intact.

## Authoring a fixture

The order of `FixtureV1.revisions` defines per-source-set chronology; revision ID spelling does not define order. Keep `fixture.json` next to the source, captured native and semantic artifacts, adjacent `*.annotations.json`, `expected/answers.json`, `expected/dispositions.json`, and `expected/anchors.json`. List every captured executable, toolchain, config, dependency and semantic artifact with its file and byte hash. Captured producers and requested comparison producers are **separate** lists; the comparison pins one admitted complete source-set snapshot. A missing requested producer does not fall back to the captured version. List every revision source file and every capability/role intent. Confine relative paths to the fixture root. Raw semantic captures carry facts and producer identity, not a self-hashing normalized basis. Independent annotations must link to captured facts; no unsupported facts may be added. Native rows carry source positions, required nullable native ID diagnostics, and explicit per-leaf source witnesses. A non-null call spelling with null callee range needs a separate spelling witness, not an invented range. Native IDs never determine generated IDs or joins; a changed raw native capture still changes the input hash.

A relationship fact requires `kind:"typeRelationship"` **and** `relationshipKind:"extends"|"implements"|"overrides"`, an internal source, target and independent provenance. No legacy nested relationship-kind substitute is accepted. Non-exact reference facts become corpus-only `referenceJoinDiagnostics`; they never install Reference occurrences, roles or bindings. Annotation identity refs are `{ref:"..."}` only in declared identity slots. Answer record refs are `{recordRef:"..."}` only in declared record slots. The answer author writes attempted requests (including invalid-limit failures), success/failure outcomes, traversal arrays, boundaries, warnings, counts and frontiers independently; the generator may resolve typed refs but must never create expected graph answers. A comparison-only `expectedGraph` may compute a reference traversal solely to compare with hand-authored answers; it must NEVER write expected answers. A malformed attempted request is an intake error, while a well-formed out-of-policy request reaches typed failure checking. Anchor attachment outcomes are also authored independently.

### Field-by-field authoring walkthrough

Start with one new directory, such as `tests/fixtures/semantic-evidence/v1/java/`. Keep the primary sources and exact producer captures from one frozen snapshot. Use `example/` for shapes, not for Java facts or precomputed answers. The schema for every nested field is in `schema.mjs`; the normative meaning and enum order are in `docs/semantic-evidence/contract-v1.md`.

1. **Describe the snapshot in `fixture.json`.** Set `formatVersion:1`, `profile:"draft"` while the corpus is being built (then `"corpus"` in its final change), and one `language` (`java`, `rust`, `python`, or `javascript`). `sourceSets[]` supplies stable `id`, `rootId`, admitted `languages` and `dependencies` (not absolute paths). `producers[]` captures each native/semantic `id`, exact `version`, `executableHash`, `kind`, `languages`, and `positionEncoding`. Pin each semantic producer's observed encoding, because SCIP indexes leave it undeclared: `utf16` for scip-typescript, scip-python and scip-java, and `utf8` for rust-analyzer (Decision 0017 Amendment 1). Semantic anchors must use that encoding. `revisions[]` is chronological and each row supplies immutable `id`, `sourceSetId`, sorted `documents[]` (`key:{sourceSetId,language,path}`, `revisionId`, `sourceFile`), plus `toolchainHash`, `configHash`, and `dependencyHash`. Paths are confined relative to the fixture root; `DocumentKey.path` is relative to the admitted source set, not to `snapshots/`. `comparison` selects one `sourceSetId`/`revisionId` and requested `producers[]` _separately_ from captured producers. Do not substitute old captured bytes for an unavailable comparison producer.
2. **Declare capture bytes and coverage requests.** `captures[]` gives each capture `ref`, `kind` (`executable`, `toolchain`, `config`, `dependency`, `semanticArtifact`), actual relative `file`, and SHA-256 `hash`. An `executable` or `toolchain` capture may instead carry `pinned:"docs/semantic-evidence/hardware/<profile>.json#<name>"` in place of `file`; the checker matches `hash` against that profile entry's `executableHash` or `toolchainHash` and stores no bytes ([Decision 0017](../../docs/semantic-evidence/decisions/0017-conformance-corpus-storage-and-producers.md)). Every producer executable and every revision's toolchain/config/dependency hash must have the right captured kind. List `nativeArtifact`, all `semanticArtifacts[]` and adjacent `annotationFiles[]`; set `answersFile`, `dispositionsFile`, `anchorCasesFile` to authored relative paths. `coverageIntents[]` identifies each producer/document/revision tuple with `requestedRoles[]` in role enum order and exactly four `measurementSupport[]` entries (`declarationName`, `callee`, `invocation`, `reference`), each `{kind,available,diagnostic,gaps}`; available means the convention is enabled and needs null diagnostic. A gap is `{range:{encoding:"utf8",start,end},diagnostic}` with a nonempty source span and concrete diagnostic. Each captured tuple needs its own coverage fact later, including omitted/failed ones.
3. **Capture native measurements in `nativeArtifact`.** The object has `formatVersion:1`, `producerId`, the required `extractionInputs` inventory (the sorted, unique non-source capture kinds `config`, `dependency`, `toolchain` the native producer reads; an explicit `[]` when it reads none, never omitted), and arrays `declarations`, `calls`, `controls`, `references`, `capabilityReports`. Production reports contain `{document,revisionId,families}` for every captured document/revision, exactly one entry per measurement kind. Native coverage intents mirror the captured families exactly. Synthetic `native@1` conformance captures may use `capabilityReports:[]` with honest authored native intent as their capability authority. Every row gets a unique `ref`, `nativeId` (including explicit `null`), `document`, `revisionId`, and measured `range:{encoding,start,end}`. A declaration also has `parentRef` (or null for top level), `kind`, `name`/`nameRange` (nullable for modules/anonymous functions), `signature` (nullable), and full `header:{kind,name,modifiers,typeParameters,parameters,resultType,bases}`. A call gets `ownerRef`, nullable `calleeRange`/`spelling`, and `regionRefs[]`. A control gets `ownerRef`, nullable `parentRef`/`arm`, and measured `kind`; a reference gets `ownerRef` and literal `spelling`. Put a `witnesses[]` entry `{field,witness:{range,text}}` for every non-null source-derived leaf, including header pieces and spelling. Ranges use the producer's declared encoding and must map to the exact source bytes. An invocation is not a call solely because a callable value was read or passed. Native IDs are diagnostics, never stable identity.
4. **Compute source-backed authoring values, then verify the capture.** For example, from the repository root:

   ```sh
   node tools/semantic-contract/author-helper.mjs hashes tests/fixtures/semantic-evidence/v1/example
   node tools/semantic-contract/author-helper.mjs ids tests/fixtures/semantic-evidence/v1/example
   ```

   `hashes` prints `snapshots[].documents[].contentHash`/`byteLength`, `sourceManifestHash` (canonical sorted document/hash pairs), and `captures[].{declaredHash,actualHash,matches}`. Use the `actualHash` for a capture after checking its bytes; a mismatch is not fixed by trusting its old declared hash. `ids` prints each native declaration's `ref`, `syntaxId`, key/ancestors, `headerHash`, `siblingGroupHash`, `siblingCount`, and `identicalHeaderCount`; each call/control/reference has `ref`, `ownerRef`, `ordinal`, `occurrenceId` (`occ:v2`, Decision 0003: from the document bytes, the extraction context of the declared inventory and the native producer descriptor, never the revision; an undeclared, unknown, duplicate or unauthenticated component fails closed as `IDENTITY.EXTRACTION_CONTEXT`). `ids` checks native positions and witnesses against source and uses the production identity functions. Fill in sources and native measurements first; `ids` does not require authored expected answers. Neither helper is a producer/parser, generates semantic facts, checks coverage, or certifies correctness; verify every measured input yourself. Both only print to standard output. Do **not** redirect output into `expected/` or `generated/`.

5. **Author independent semantic and annotation facts.** Each raw `semanticArtifacts[]` file has `formatVersion:1`, `producerId`, `facts[]` captured from that producer; do not put the normalized `SemanticBasis` inside bytes whose hash that basis will name. A `snapshots/<revision>/.../<source>.annotations.json` file beside its declared source has `formatVersion:1`, matching `document`/`revisionId`, independently anchored `scenarios[]` (`id`, `category`, `anchors[]`, `factRefs[]`), and `facts[]`. Use `coverage` facts with `record` carrying producer/language/source-set/document/revision, `requested`, `selected`, `state`, `supportedRoles`, `observedRoles`, `diagnostic`. Use `provenance` facts with captured document bytes, semantic basis and evidence kind, then source-linked `declarationBinding`, `symbol`, `reference`, `callBinding`, or `typeRelationship` facts with matching proof IDs. Select `AnchorSelector` by exact document/revision/contentHash/kind/range/**ownerRef**; do not guess a join by spelling or native ID. Semantic target refs use `{kind:"internal",declarationRef,revisionId}` or explicit external symbol keys. Distinct conflicting exact targets retain separate proofs and become ambiguous; non-exact reference joins stay diagnostics, not invented occurrences.
6. **Author expected checks independently.** `expected/dispositions.json` has `formatVersion:1`, `assertions[]` (explicit join statuses and resolution dispositions keyed by `factRef`) and `callableValueNegatives[]` (`scenarioId`, `referenceRef`, `ownerRef`, `range`). `expected/anchors.json` has `formatVersion:1`, `cases[]` with `id`, `capturedDeclarationRef`, `currentRevisionId`, `continuity:{fromRevisionId,toRevisionId,state,evidence}`, and independent `expectedResult:{status,targetId,reason}`. `expected/answers.json` has `formatVersion:1`, `answers[]` with `id`, `attemptedRequest:{sourceSetId,revisionId,rootSyntaxId,semanticProducerId,depth,maxNodes,maxCalls}` and an authored `answer`. A failure is `{ok:false,error:{code,message,field}}`; a success has `ok:true` and a result with `request`, `resolvedRevisionId`, ordered `nodes`, `edges`, `frontier`, `coverage`, `provenance`, `partial`, `truncated`, `warnings`. For each edge write `call`, `from`, nullable `to`/`binding`, `visit`, `boundaryReason`; enumerate limits and boundaries rather than asking the checker to fill them. `{ref:"native-ref"}` is accepted only in declared ID slots, and `{recordRef:"fact-or-native-ref"}` only in declared answer-record slots. For a failed/omitted current semantic tuple, never promote an old call binding to the current call: it is a `binding:null`, `boundaryReason:"missingEvidence"` boundary. Historical declaration proof follows the per-document latest eligible tuple rule in the contract; `GraphResult` has no extra field for that history.
7. **Validate before publishing.** Run `node tools/semantic-contract/cli.mjs generate --fixture java --check` against an existing publication, or `generate --fixture java` only after authoring all checks to publish verified bytes. Then `node tools/semantic-contract/cli.mjs check --fixture java` and `node tools/semantic-contract/test/run.mjs`. The runner discovers immediate corpus directories. Never copy computed traversal into authored answers, and never edit generated bundles.

`generated/manifest.json` is the **only** publication pointer. All files in `generated/` are **NEVER HAND EDITED**. Generated bundles have content-addressed `bundles/<bundleHash>/{records,answers,counts}.json` paths, #22 canonical bytes and no trailing newline. Validate source, records, freshness, graph, warning keys, dispositions and floors before writing anything. A new bundle is written to a temporary directory and renamed into `bundles/<bundleHash>`; an existing bundle must already hold identical bytes. The manifest is renamed into place **last**, so a failed generation leaves the previous publication intact. `inputHash` includes independently authored expected bytes and every transitively declared input. Neither generated bytes nor environment/timestamps enter it.

The `example/snapshots/{r0,r1,r2}/src/anchors.js` sources are intentionally classic JavaScript scripts, not ES modules. Duplicate sibling function declarations are valid in scripts, while exporting those duplicate names from an ES module is a syntax error. The example tests run native `node --check` on each snapshot and reject the duplicate-export regression.

The literal `example/` is the **sole** `profile=example` exemption from corpus count floors, with language `javascript`; it still passes every other invariant. Add `java/`, `rust/`, `python/`, or `javascript/` as corpus-profile fixture directories using **language-owned data only**, not a plugin, shared test registry or code change. Each directory has an immediate `fixture.json` with matching language and `profile:"corpus"` or `profile:"draft"`; only `example/` uses `profile:"example"`, and it cannot be `draft`. Discovery and every CLI `generate`/`check` enforce this directory, profile and language pairing. A **draft** language corpus passes every invariant except the count floors, and the runner labels it `DRAFT`. Use it for incremental slices only: the final change to each corpus sets `profile:"corpus"`, which enforces the full floors. Stage 1 ratification (#31) requires every language corpus to be `corpus`. The runner discovers these directories automatically, rejects missing declarations, and reports fixture paths on failure. The example and JavaScript corpus remain separate.

### Corpus count floors

#299 does **not certify** the 40-reference, 20-callable-value-negative or four imports/aliases scenario floors. Certification is deferred to corpus issues #27–#30 after #88/#89 joins real SCIP facts to native-v10 candidates; the required totals are unchanged.

The floors are per-language data in `counts.mjs`, as amended by Decision 0017 [Amendment 1](../../docs/semantic-evidence/decisions/0017-conformance-corpus-storage-and-producers.md#amendment-1-2026-10-23-outcome-floors) and [Amendment 3](../../docs/semantic-evidence/decisions/0017-conformance-corpus-storage-and-producers.md#amendment-3-2026-10-23-production-native-captures). The owner-ratified [Amendment 4](../../docs/semantic-evidence/decisions/0017-conformance-corpus-storage-and-producers.md#amendment-4-2026-10-24-production-capability-reports) defers the JavaScript unsupported floor to 0. Every full corpus needs 32 independently anchored scenarios (four per category), 120 measured calls, 40 references, 20 callable-value negatives, 20 each of resolved, proven-external and unresolved outcomes, and every applicable role. The rest differ by language:

| Floor | JavaScript | Java | Python | Rust |
|---|---:|---:|---:|---:|
| ambiguous outcomes | 20 | 0 | 0 | 0 |
| type relationships | 20 | 20 | 20 | 0 |
| unsupported documents | 0 | 0 | 0 | 1 |
| alias role required | yes | no | no | yes |

- **unsupported** counts documents, not anchors: a document/revision counts once when its production capability report has a genuine family gap or wholly unavailable convention and that tuple carries at least one authenticated captured semantic producer fact (including `symbol`; excluding `coverage` and `provenance` bookkeeping). Authored false flags, native partial coverage and asserted unsupported joins are not production authority. The fact need not lie in the gap or use its family. Synthetic conformance captures retain honest authored native capability intent as their authority. JavaScript, Java and Python have a floor of 0 until a genuine construct is found and a further amendment restores it; Rust remains at 1.
- **unresolved** is authored as a `callBinding` (or `reference`) fact with `resolution:"unresolved"`, `declaredTarget:null` and no candidates, on the exact join of a measured call or reference that has no producer occurrence, in a document whose semantic coverage tuple is `complete`. SCIP has no positive unresolved record, so absence at an exact measured join in a complete document is the evidence. The checker rejects an `unresolved` resolution disposition (`COUNT.DISPOSITION`) unless that producer's coverage tuple for the document is `complete`.
- **alias** is satisfied by a reference fact at the measured alias name (`site:"declaration"`, roles `definition` and `alias`) whose target is the original declaration; that is what the producers report at the alias name. Java and Python are exempt from the alias floor.

The focused temporary Java corpus uses actual `this::target` callable reads, imported `java.util` names, an assigned field, a generic type use, recursive calls, externally declared `java.util.Objects.requireNonNull`, and ambiguous `String`/`Integer` overload calls. The ambiguous `ambiguous(null)` and unknown `unknown()` calls are parseable Java with deliberate compilation errors; they are not compile-success examples. The partial native capture measures 120 calls by callee but cannot match invocation-family positions. It omits 20 additional syntactically valid `this.target()` invocations; only those omitted expressions receive independently authored unsupported-join dispositions. Its measured calls and unsupported invocations are separate source occurrences. Do not count unsupported labels, repeated facts, non-exact references or producer copies as occurrences.

## Fixture policy

These rules are enforced by the tooling but are not stated in the #22 contract. They constrain fixtures, not the product.

- **`omitted` vs `unsupported`.** A requested tuple that is not selected is `unsupported` when no requested role is both supported and measurable for that document, and `omitted` otherwise (`record-check/coverage.mjs`).
- **Anchor cases.** Every durable-anchor case's `currentRevisionId` must be the comparison revision, and its captured document must exist there. Otherwise the case is rejected as `ANCHOR.TUPLE` rather than reported `missing`.
- **Facts need an observing tuple (`COVERAGE.FACT`).** A semantic fact is rejected when its tuple is unselected or `failed`, or when its role is not in the tuple's `observedRoles`, even if no answer uses it.
- **Call role mapping.** The `call` role is measurable when either the `callee` or the `invocation` measurement family is available.
- **Freshness components.** Besides contract rule 2's components, freshness also compares the requested producer's `positionEncoding` and `languages` with the captured producer. A mismatch can only make evidence `possiblyStale`, never fresher.
- **Independent re-derivation.** `normalize.mjs` labels records using `check-freshness.mjs`. `record-check/*` re-derives joins, coverage and freshness separately so normalized rows are never their own oracle. Keep the two separate.

## Mutation controls and review

Tests define local finite control rows `{id,baseline,mutate,expectedAssertion,expectedCode,expectedField}` and pass them to `test/mutations.mjs`. The harness first validates baseline, then applies one mutation and requires the exact assertion/code/field; new slices add their own rows without changing a central inventory. `schemaFieldControls` derives required-field omission rows from the registry. Assertions use stable prefixes such as `FORMAT.*`, `RELATIONSHIP_FORMAT.*`, `CANONICAL.*`, `ID.*`, `COVERAGE.*`, `FRESHNESS.*`, `JOIN.*`, `GRAPH.*`, `WARNING.*`, `COUNT.*`, and `PUBLICATION.*`; these are tooling assertions, **not** extra #22 semantic/error/warning codes. A mutation reaching an unrelated shape/hash error does not pass its control. Keep independently authored expected files separate from checker logic: a green test must never compare the checker to an answer it generated itself.

## Capture with the production native extractor (#95)

The registry admits the current `trellis.native.syntax@native-v10` with the
production `EXTRACTION_INPUTS` inventory (`[]` for Java, Rust, Python and
JavaScript). Historical v3/v4 descriptors are not admitted. The Rust inventory
guard queries the complete production entry set in this JS registry through Node
and requires equality with the actual Rust table; missing, extra, version,
language or component drift fails verification. A new production version replaces
the old production entry. Historical versions are not retained, and an existing
version's extraction-input claim is never changed.

Build once on `rust-toolchain.toml` in the owning checkout and an isolated target:

```sh
cargo build --locked --features test-support --bin trellis
"$CARGO_TARGET_DIR/debug/trellis" native-capture --describe
"$CARGO_TARGET_DIR/debug/trellis" native-capture \
  --fixture /absolute/path/to/fixture/fixture.json \
  --output /absolute/path/to/fresh-native.json
```

`native-capture` is a supported **test-support-only** authoring command, not an
index/daemon operation. It reads the fixture's source-set/revision/document lists
and captured source bytes, invokes the same native measurement code as the
production indexer, and writes the toolkit's raw `NativeArtifact` shape. It does
not read semantic artifacts or expected answers, run repository code, resolve
dependencies, access the network or publish a store. An optional `--source-set`
and `--revision` restrict the selected snapshot; without them all listed revisions
are captured. Output must be a fresh file. Repeating a capture of the same bytes
and descriptor yields identical output. The existing example fixture remains
unchanged; the integration test captures its representative sources in a temporary
native-only draft and normalizes/checks those measured rows. Production `partial`
document coverage is printed to stderr with its document/revision and diagnostic;
the raw `NativeArtifact` shape has no coverage field. Capture does not turn
partial coverage into complete coverage or a per-family unavailable claim.

`--describe` reports the actual compiled producer descriptor, UTF-8 encoding,
per-language extraction inventory and `hashKind:"nativeExtractor"`. The descriptor
uses the compiled `TRELLIS_EXTRACTOR_HASH`, including the `test-support` build flag,
not an environment override or the whole executable's SHA. Record that descriptor
and the pinned Rust toolchain/source provenance with the capture. Do not copy a
production-build hash onto a test-support build. The hash covers the fixture
adapter (`src/native_capture.rs`) as well as the production extraction modules
only when `test-support` is enabled. Witness/conversion edits change the capture
pin but do not change the release hash. It identifies this
test-support capture build, not a release binary. Decision 0017
[Amendment 3](../../docs/semantic-evidence/decisions/0017-conformance-corpus-storage-and-producers.md#amendment-3-2026-10-23-production-native-captures)
pins production native captures by this hash; no real hardware pin exists yet.

The adapter preserves production declarations, calls, controls, identifier-site reference
candidates and per-document capability reports. It does not reparse source, infer
semantic roles or targets, or turn identifier candidates into calls. Reference IDs
and ordinals come from the full native inventory, not a semantic producer's subset.
A declaration-name candidate uses its declarationName owner's `parentRef ?? self`.
Only the ratified JavaScript explicit named-import and Rust explicit use-as aliases
gain narrow measured alias declarations. The toolkit accepts unnamed Rust
`implementation` declarations and treats the synthetic module as outermost when
its range equals a real declaration's range; nested declarations still require
the real immediate source parent. Witnesses attest the actual header/spelling
leaves against the captured UTF-8 source, and native IDs are diagnostic labels,
never generated stable IDs. A field that cannot be witnessed, or a production signature inconsistent with
its measured header (currently possible for Java varargs), fails capture rather
than being silently rewritten. Fixing those production measurements requires its
own producer-version decision; the adapter must not invent missing types. Keep annotations/coverage honest about the families present. Native-v10 enables all
four conventions, including in empty documents. Actual opaque constructs are
reported as narrow per-family gaps, never as fabricated disabled working families.

A join is unsupported only if its entire half-open anchor lies inside one reported
gap and no measured candidate matches. Crossing or merely touching a gap is not
unsupported. Same-family candidates wholly inside a reported gap invalidate the
capture, even without semantic facts there. Enclosing invocations and valid
outside anchors remain usable. Normalizer and checker derive these rules
independently from source coordinates and captured reports. A requested native
family gap makes native coverage partial with a diagnostic without erasing
supported/observed roles. It does not by itself falsify complete semantic producer
coverage. Production reports must be complete and native intents must mirror them;
missing reports and authored overrides fail closed.


## Combined performance-gate preparation (#61)

Method and unresolved launch decisions: [performance-gate-method.md](../../docs/semantic-evidence/performance-gate-method.md).
This preparation does not collect timings or establish a benchmark pass.

```sh
python3 tools/semantic-contract/generate-performance-scip.py \
  --cohort-root /path/to/generated-cohorts \
  --cohort-manifest tools/synthetic-cohorts/manifest-v2.json \
  --size large --out /fresh/path/scip-preparation
python3 tools/semantic-contract/qualify-performance.py \
  --report /path/to/raw-report.json --out /fresh/path/offline-evaluation.json
python3 -m unittest discover -s tools/semantic-contract/test -p 'test_performance*.py' -v
```

The generator verifies the exact selected tier and writes deterministic synthetic
binary SCIP plus `preparation-manifest.json`. Its inventory is **unbound, not a
closed admission manifest**. It asserts no native joins or actual reference
producer execution. Full LARGE output exceeds existing admission document/fact
bounds and cannot currently be imported as one language artifact. Lower density
alone cannot fix the complete 10k source-inventory manifest cap. The reviewed
#90 staged representative import/rejoin alternative also needs an explicit scope
and interval decision; nothing here bypasses admission or changes caps.

The qualifier checks declared raw samples against the existing policy. Input
`reportVersion:1` selects `tier`, reviewed `runCount` (no default), all true review
flags (`methodReviewed`, `dependencies90Merged`, `featureMerged`, `hostRatified`)
and runner receipt flags (`rawAdmissionValidated`, `provenanceValidated`,
`canonicalEvidenceValidated`). Every sample needs a unique `sampleId`, positive
`durationSeconds`, integer `peakRssBytes`, exact `files`/`sourceBytes`,
`nativeFactsOverall` equal to the four `nativeFactsPerLanguage` counts, plus
`coherentNativeSemantic:true` and `requestedCoverageSatisfied:true`. MEDIUM/LARGE
also require integer `steadySqliteBytes`, `peakDatabaseWalShmBytes` and
`canonicalSerializedEvidenceBytes`. An incomplete/faulted set is `notMeasured`,
not a partial passing median. Threshold failures remain `failed`.

Flags are declarations of external receipts, not receipt verification. Every
output says `offlineEvaluationOnly:true` and `authoritativeAcceptance:false`.
The actual combined measurement adapter, authenticated receipt validation,
RSS/peak collection and current-schema native+semantic denominator remain
unfinished pending integration and method review. Reuse accepted #90 support;
do not duplicate its daemon lifecycle or count its incremental p95 as this gate.
