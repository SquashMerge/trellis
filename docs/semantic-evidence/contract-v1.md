# Semantic evidence and graph-answer contract, v1

**Status:** prospective, review-complete contract for ratification. It is not a deployed API, storage schema, or production-ID migration. **Proposed amendment:** [Decision 0003](decisions/0003-per-document-occurrence-ids.md) makes occurrence identity per document version and extraction context (`occ:v2`, the only occurrence form) and moves the "never crosses revisions" guarantee onto revision-scoped semantic validity.

This contract describes static evidence measured from one immutable source snapshot. It never claims an execution trace, runtime reachability, or a runtime-complete call graph. All fields are required. A `?` means the field is present and may be `null`; arrays are present and non-null. A v1 consumer rejects unknown fields.

## Conventions and shared types

| Type | Shape and invariant | Failure/disposition |
|---|---|---|
| `Text` | Nonempty valid Unicode scalar text, UTF-8 encoded; case-sensitive; no lone surrogate. | Invalid/empty text invalidates its containing artifact. |
| `Language` | `java \| rust \| python \| javascript`. | Other value is invalid. |
| `UInt` | Integer in `[0, 2^53-1]`. | Fractions, negatives, and larger values are invalid. |
| `Hash` | Exactly 64 lowercase hexadecimal characters. | Wrong spelling/length is invalid; a verified mismatch is `invalidDigest`. |
| `Path` | Nonempty source-root-relative POSIX path. Preserve bytes: no leading slash, empty/`.`/`..` segment, backslash, or NUL; no case or Unicode normalization. | Invalid or escaping path is a hard failure. |
| `Range` | `{start:UInt,end:UInt}`; zero-based half-open UTF-8 byte offsets on scalar boundaries, normally `start < end <= byteLength`. Only an empty module declaration may be empty. | Out-of-bounds, reversed, or non-boundary positions are `invalidRange`; they are not ambiguous joins. |
| `Kind` | `module \| namespace \| type \| implementation \| function \| method \| constructor \| field \| variable \| parameter \| typeParameter \| alias \| anonymousFunction`. It names measured syntax, not universal language syntax. | Do not synthesize a declaration to fill a kind. |
| `SyntaxId` | `sid:v1:` followed by exactly the first 32 lowercase hex characters (first 16 bytes) of the full declaration SHA-256 digest. | Wrong spelling/length or a detected handle collision (same namespace and first 16 digest bytes, different full SHA-256 digest) invalidates the affected artifact; see [Decision 0015](decisions/0015-native-hash-based-identity.md). |
| `OccurrenceId` | `occ:v2:` followed by exactly the first 32 lowercase hex characters (first 16 bytes) of the full occurrence SHA-256 digest ([Decision 0003](decisions/0003-per-document-occurrence-ids.md)). There is no other occurrence form. | Wrong spelling/length or a detected handle collision (same namespace and first 16 digest bytes, different full SHA-256 digest) invalidates the affected artifact; see [Decision 0015](decisions/0015-native-hash-based-identity.md). |
| `Resolution` | `resolved \| external \| ambiguous \| unresolved`; cardinalities are defined under bindings. | Impossible target cardinality invalidates the artifact. |
| `Role` | `definition \| read \| write \| call \| type \| import \| alias`, in this enum order. | Unknown, duplicate, out-of-order, or non-applicable role is invalid. |

A document-level measured `module` owns top-level occurrences. Records resolve within their stated source set and revision except for explicit external targets. Where this contract says “sorted,” language values use the `Language` declaration order, enum values use their declared order, text/path uses unsigned UTF-8 byte lexicographic order, IDs use ASCII byte order, and structured `Target` values use their canonical JSON bytes. Tuple comparison is left-to-right; nullable tuple members put null before text. These rules do not reorder ordinary measured arrays unless a sort is explicitly required.

## Identity and coverage

| Record.field | Type / nullability | Invariant | Failure or disposition |
|---|---|---|---|
| `Producer.id` | `Text` | Stable nonempty producer identity. | Invalid text rejects descriptor. |
| `.version` | `Text` | Exact captured version. | Missing/invalid rejects descriptor. |
| `.executableHash` | `Hash` | Digest of the executable bytes, captured in the fixture or attested by the reference hardware profile ([Decision 0017](decisions/0017-conformance-corpus-storage-and-producers.md)). | Invalid/mismatch rejects descriptor. |
| `.kind` | `native \| semantic` | Native measures syntax; semantic supplies semantic facts. | Other value rejects descriptor. |
| `.languages` | `Language[]` | Nonempty and unique. | Empty/duplicate rejects descriptor. |
| `.positionEncoding` | `utf8 \| utf16 \| unicodeScalar` | Producer metadata, never a common-layer assumption. | Unsupported encoding or an unconvertible position rejects the artifact before publication. |
| `SourceSet.id` | `Text` | Stable admitted logical identity, not an absolute machine path. | Invalid/unknown ID fails admission. |
| `.rootId` | `Text` | Stable admitted root identity. | Unknown root is denied. |
| `.languages` | `Language[]` | Nonempty, unique admitted languages. | Invalid list rejects descriptor. |
| `.dependencies` | `Text[]` | Unique admitted source-set IDs; conveys no traversal authority. | Duplicate/unknown dependency rejects descriptor. |
| `DocumentKey.sourceSetId` | `Text` | Identifies its admitted source set. | Unknown source set is a hard failure. |
| `.language` | `Language` | Admitted by source set. | Mismatch rejects key. |
| `.path` | `Path` | Resolves inside the admitted root with spelling preserved. | Escape/invalid path is a hard failure. |
| `Document.key` | `DocumentKey` | Unique within revision. | Duplicate key invalidates revision artifact. |
| `.revisionId` | `Text` | Equals containing immutable revision. | Mismatch invalidates artifact. |
| `.contentHash` | `Hash` | SHA-256 of exact source bytes with empty domain prefix. | Digest mismatch invalidates artifact. |
| `.byteLength` | `UInt` | Exact byte length of those source bytes. | Mismatch invalidates artifact/ranges. |
| `Revision.id` | `Text` | Immutable complete snapshot identity, including dirty/non-Git content. It covers the source documents, toolchain, config and dependency captures and the native extractor; it never includes semantic producers or their artifacts, which rule 2 compares through each proof's `SemanticBasis`. | Mutation or mismatch makes revision unavailable/invalid. |
| `.sourceSetId` | `Text` | One admitted source set. | Mismatch rejects revision. |
| `.documents` | `Document[]` | Complete, unique by key, sorted by language then path. | Missing/duplicate/unsorted rows invalidate revision. |
| `.toolchainHash` | `Hash` | Captured toolchain artifact digest, not a timestamp. | Missing/mismatch invalidates basis use. |
| `.configHash` | `Hash` | Captured configuration digest. | Missing/mismatch invalidates basis use. |
| `.dependencyHash` | `Hash` | Captured dependency/manifests digest. | Missing/mismatch invalidates basis use. |
| `Coverage.producerId` | `Text` | Existing producer. | Dangling producer invalidates artifact. |
| `.language` | `Language` | Supported/admitted tuple language. | Mismatch invalidates row. |
| `.sourceSetId` | `Text` | Tuple source set. | Unknown/mismatch invalidates row. |
| `.documentPath` | `Path` | Tuple document path. | Missing document invalidates row. |
| `.revisionId` | `Text` | Tuple immutable revision. | Mismatch invalidates row. |
| `.requested` | `boolean` | State tuple rule below. | Impossible tuple invalidates row. |
| `.selected` | `boolean` | State tuple rule below. | Impossible tuple invalidates row. |
| `.state` | `notRequested \| omitted \| unsupported \| failed \| partial \| complete` | `notRequested=(false,false)`; omitted/unsupported=`(true,false)`; failed/partial/complete=`(true,true)`. | Any other combination invalidates row. |
| `.supportedRoles` | `Role[]` | Unique, enum ordered, applicable roles this producer supports. | Invalid or non-applicable role invalidates row. |
| `.observedRoles` | `Role[]` | Unique subset of supported and applicable roles. | Non-subset invalidates row. |
| `.diagnostic` | `Text?` | Null only for `notRequested` or `complete`; otherwise required. | Wrong nullability invalidates row. |

Coverage is scoped to the complete `(producerId,language,sourceSetId,documentPath,revisionId)` tuple. Publish one row for every tuple, including selected, omitted, unsupported, failed, and not-requested documents. Two producers covering one document have two rows. `complete` means all requested, supported evidence for that document; it never means universal language support. A requested unsupported role makes the row `partial` with a diagnostic. Non-applicable roles are excluded rather than reported missing.

## #279 amendment: production measurement capabilities

**Status: ratified by the owner, 2026-10-24.** This section defines native-v9's capability and join rules. [Decision 0017's Amendment 4](decisions/0017-conformance-corpus-storage-and-producers.md#amendment-4-2026-10-24-production-capability-reports) defines the corresponding corpus count authority. Other join, semantic resolution and publication rules remain unchanged.

The production native artifact carries one capability report for each `(DocumentKey,revisionId)`, with exactly one entry for each of `declarationName`, `callee`, `invocation` and `reference`. Each entry is `{kind,available,diagnostic,gaps}`, where each gap is `{range,diagnostic}`. The document, revision and ranges use the existing shared types. A gap range is a nonempty, parser-measured UTF-8 scalar-boundary span of the actually opaque source construct, within that document's exact bytes. Its nonempty diagnostic identifies the construct and the missing measurement convention. The enclosing native artifact supplies the producer and source-byte authority.

`available` means the family convention is enabled for the document, not that it emits a row or measures every construct. An enabled family has `diagnostic:null` and may have gaps. A wholly unavailable family has a nonempty family diagnostic and no candidates or gaps; no synthetic whole-file range is needed. Native-v9 enables all four families, including in empty valid documents. Ordinary opaque Rust constructs are range-scoped gaps, not disabled families. A working family must not be disabled to meet a floor.

Production reports are validated and published with native evidence, then preserved through reuse, retained reconstruction and test-support capture. A production document missing its report is invalid, not unavailable. Fixture intents may mirror the report but never override it. There is no exporter-only classification, second producer, new public result/error enum or semantic proof derived from a capability report.

Validate the complete anchor tuple and convert/validate coordinates against the exact source **before** classifying capability. In an enabled family, an anchor is `unsupported` only if its entire half-open range is contained in one same-family gap and no compatible measured candidate exists. It retains `candidateIds:[]` and that concrete gap diagnostic. A same-family measured candidate wholly contained in a claimed gap contradicts the report and rejects the artifact; never discard the candidate or prefer unsupported over it. Elsewhere the existing exact/ambiguous candidate-cardinality rules apply. An anchor with no candidate outside a gap is `unmatched`, including one merely overlapping or crossing a gap boundary. A measured invocation enclosing a nested gap is not suppressed by overlap. A wholly unavailable convention retains `unsupported` with its concrete family diagnostic. Invalid coordinates remain invalid, never unsupported.

Report only affected families at actual skipped opaque subtree spans, not the surrounding function, class or file for convenience. Rust macro/attribute token-tree skips must name the skipped spans and missing conventions. Parser recovery or whole-document partial coverage alone proves no gap. A grammar kind excluded by the measurement convention does not become a gap merely because it exists. Do not fabricate syntax, remove working measurements or infer a gap from an authored unsupported join.

Valid declaration-name, callee, invocation and reference anchors beside or around a gap remain usable. Keep supported and observed roles for valid evidence outside gaps; `available:true` continues to permit those roles. A selected native coverage tuple is `partial` with a diagnostic when a gap affects its requested evidence. This is independent of semantic completeness: a native gap alone does not falsify an authenticated semantic producer's complete raw-artifact coverage. Inferred unresolved outcomes still require that existing semantic completeness authority. An unsupported reference join installs neither a semantic `Reference` nor its roles.

## Basis and freshness

| Record.field | Type / nullability | Invariant | Failure or disposition |
|---|---|---|---|
| `SemanticBasis.producerId` | `Text` | Captured semantic producer; self-matches the provenance and producer descriptor. | Missing, malformed, dangling, or self-mismatching value rejects the semantic artifact atomically. A valid captured producer unavailable in the requested comparison is `possiblyStale`. |
| `.producerVersion` | `Text` | Exact captured version; self-matches the captured producer descriptor. | Missing, invalid, or self-mismatching value rejects the artifact. A valid captured value different from or unavailable in the requested comparison is `possiblyStale`. |
| `.producerHash` | `Hash` | Digest of the executable bytes, captured in the fixture or attested by the reference hardware profile ([Decision 0017](decisions/0017-conformance-corpus-storage-and-producers.md)); self-matches those bytes or that attestation and the captured producer descriptor. | Missing, malformed, or digest-self-mismatching value rejects the artifact. A valid captured digest different from or unavailable in the requested comparison is `possiblyStale`. |
| `.artifactHash` | `Hash` | Digest of declared captured semantic artifact bytes, empty prefix. | Missing, malformed, or digest-self-mismatching value rejects the artifact atomically; it is not a freshness comparison. |
| `.language` | `Language` | Captured language; self-matches facts and producer. | Missing, malformed, or self-mismatching value rejects the artifact. A valid captured language not available for the requested comparison is `possiblyStale`. |
| `.sourceSetId` | `Text` | Captured admitted source set; self-matches the facts. | Missing, invalid, dangling, or self-mismatching value rejects the artifact. A valid captured source set unavailable in the requested comparison is `possiblyStale`. |
| `.revisionId` | `Text` | Captured semantic revision identity; resolves to the captured revision and self-matches the facts. | Missing, invalid, dangling, or self-mismatching value rejects the artifact. A valid captured revision different from or unavailable in the requested comparison is `possiblyStale` when evidence-document bytes are identical. |
| `.sourceManifestHash` | `Hash` | Digest, empty prefix, of the captured canonical JSON array `{document:DocumentKey,contentHash:Hash}` sorted language then path. | Missing, malformed, or digest-self-mismatching value rejects the artifact. A valid captured manifest digest different from or unavailable in the requested comparison is `possiblyStale`. |
| `.toolchainHash` | `Hash` | Digest of declared captured toolchain bytes, empty prefix. | Missing, malformed, or digest-self-mismatching value rejects the artifact. A valid captured digest different from or unavailable in the requested comparison is `possiblyStale`. |
| `.configHash` | `Hash` | Digest of declared captured configuration bytes, empty prefix. | Missing, malformed, or digest-self-mismatching value rejects the artifact. A valid captured digest different from or unavailable in the requested comparison is `possiblyStale`. |
| `.dependencyHash` | `Hash` | Digest of declared captured dependency/manifest bytes, empty prefix. | Missing, malformed, or digest-self-mismatching value rejects the artifact. A valid captured digest different from or unavailable in the requested comparison is `possiblyStale`. |
| `.lookupDependencies` | `Text[]` | Captured sorted unique language lookup keys, never stable-ID spellings. | Missing, invalid, duplicate, unsorted, or self-inconsistent list rejects the artifact; a well-formed list is not used for precision freshness in v1. |
| `Provenance.id` | `Text` | Unique evidence identity. | Duplicate conflict invalidates artifact. |
| `.producerId` | `Text` | Resolves to producer. | Dangling ID invalidates artifact. |
| `.document` | `DocumentKey` | Evidence document. | Outside tuple invalidates artifact. |
| `.revisionId` | `Text` | Captured revision for captured evidence; a publication-rejoined proof uses destination revision with the verified `derivedFrom` exception below. | Dangling/mismatch invalidates artifact. |
| `.contentHash` | `Hash` | Captured exact document bytes for captured evidence; publication-rejoined proof names verified identical destination bytes. | Changed source establishes stale. |
| `.evidenceKind` | `measuredSyntax \| declarationBinding \| semanticReference \| typeRelationship` | Describes evidence, not an explanation. | Other kind invalid. |
| `.basis` | `SemanticBasis?` | Null only for native measured syntax; semantic facts require matching semantic producer and non-null basis. A captured r1 basis in minted r2 provenance is allowed only with a verified `derivedFrom` link. | Wrong pairing invalidates artifact. |
| `.freshness` | `fresh \| possiblyStale \| stale` | Derived exactly by rules below; no numeric confidence. | Incorrect claimed state rejects selection/use. |
| `.derivedFrom` | `{provenanceId:Text,recordKind:occurrence\|symbolInformation\|relationship,factDigest:Hash}?` | Explicit null on captured provenance (including native); minted publication-rejoined semantic provenance uses an authenticated, uniquely resolved captured raw envelope and full digest. | Unknown/wrong kind, missing link on minted proof, unverified captured proof ID/digest or contradictory destination/basis rejects producer artifact atomically. |

Freshness is independent from coverage and is evaluated only after captured-basis validity succeeds. First validate that every required `SemanticBasis` field is present and well-formed, every captured digest matches the captured bytes it claims to digest, and every captured cross-field relation self-matches. Any failure rejects the semantic producer artifact atomically; it never produces a freshness label. Then compare that valid captured basis with the requested snapshot:

1. Changed or missing requested source bytes for the evidence document is `stale`; its overlay is not applied to current syntax.
2. With identical evidence-document bytes, a valid captured semantic revision identity, source manifest, toolchain, config, dependency, producer version/hash, producer, language, or source set that differs from or cannot be obtained in the requested comparison makes the evidence `possiblyStale`. This is conservatively source-set-wide; lookup dependencies do not narrow it in v1.
3. Only a match of every comparison component is `fresh`. Native syntax needs matching measured bytes/revision and no invented semantic basis.

A failed refresh creates a `failed` coverage row and retains the old provenance as labelled historical evidence. It never relabels the old basis fresh and never binds old evidence to another revision's occurrences.

<a id="historical-evidence-scope"></a>
**Historical evidence scope (clarification, 2026-09-25).** This adds no field and no output record. `GraphResult` returns no `Symbol`, `DeclarationBinding` or `TypeRelationship` records. Retained declaration facts only select which existing `Provenance` rows an answer returns; the latest eligible earlier producer/document tuple supplies its captured `Coverage` row whether or not a fact names the returned declaration. The facts are never applied to the answer. Stable syntax IDs, and occurrence IDs of a byte-identical document at the same key under the same native producer and extraction context ([Decision 0003](decisions/0003-per-document-occurrence-ids.md)), are the only cross-revision identity links; neither carries semantic validity across revisions.

Take a returned `GraphNode.declaration` with syntax ID `S` in document `D`, an answer at revision `r2`, and a selected producer `P` whose `r2` tuple for `D` is `failed` or `omitted`. Let `r1` be the latest earlier revision at which `P`'s tuple for `D` is `complete` or `partial`; if none exists, nothing historical is returned. The answer returns:
- the provenance of **every** `P` fact captured at `r1` whose provenance document is `D` and which names `S`, meaning a `DeclarationBinding.syntaxId`, an internal `Symbol.declarations` target, or a `TypeRelationship.source`. A fact whose evidence document is not `D` is excluded even if it targets `S`;
- `P`'s captured `r1` coverage row for `D`, beside the required `r2` row.

The `r1` tuple supersedes every earlier one for `(P,D)`. Its coverage row is returned even when no `r1` fact names `S`: the latest complete or partial analysis reported nothing for `S`. The checker must never fall back to an older revision's proof for `S`. Here `complete` has its coverage meaning: complete for the requested, supported roles, not every possible fact. A `partial` `r1` row signals its uncertainty through `coverageIncomplete`, not through older history.

Each such provenance keeps its captured revision. Its freshness compares the captured bytes of `D` with the requested `r2` bytes of `D`:
- unchanged bytes are `possiblyStale`, because the captured revision differs;
- changed or missing bytes are `stale`.

warnings-v1 applies unchanged to the returned rows: a selected `failed` or `partial` row triggers `coverageIncomplete`, while an unselected `omitted` row alone does not; stale and possibly-stale provenance trigger `staleEvidence`.

Occurrence-keyed semantic evidence never crosses revisions. A `CallBinding`, a `Reference` resolution or target, and any join over occurrence candidates is valid **only at the revision of its provenance**: the captured revision for captured evidence, the destination revision for publication-rejoined evidence. Since [Decision 0003](decisions/0003-per-document-occurrence-ids.md), a byte-identical document at the same `DocumentKey`, under the same native producer ID, version and extraction context keeps its occurrence IDs across revisions, so this is enforced by that revision scope and **not** by the ID: a shared occurrence ID never carries a binding, resolution, target, join or provenance from one revision to another. A pinned read of revision r2 projects every native record of a reused document version as r2 evidence (r2 `.revisionId`, r2 native measured-syntax provenance and coverage); no r1 provenance, resolution, target or join becomes r2-valid because an occurrence ID matches. After a failed `r2` refresh, `r2` calls have no selected-producer binding, even when their IDs equal `r1`'s, and remain boundaries. Return no historical provenance or coverage for:
- declarations the answer does not return;
- other documents or other producers;
- revisions other than `r1`.

Provenance selected merely because it exists, or that selects itself, is invalid.

`CallBinding.staleTarget` is null when there is no internal declared target and false for a uniquely verified same-revision measured internal target. Publication-rejoined A may point to changed B only if its r2 measured declaration uniquely verifies the captured stable syntax ID; then A provenance is `possiblyStale`, target revision is r2 and `staleTarget=false`. A missing/nonunique B target omits only dependent A facts and makes A partial. `staleTarget=true` and its warning remain invalid in v1, including this amendment. `possiblyStale`, `stale`, or `staleTarget=true` forbids expansion.

Hand-checks:

| Case | Coverage | Freshness / target result |
|---|---|---|
| Producers P1 and P2, one document | Two independent tuple rows; P1 complete does not fill P2 omitted/failed. | Each basis is compared independently. |
| Partial but fresh | `partial` with diagnostic and exact observed-role subset. | `fresh` if every captured basis component matches. |
| Complete but stale | `complete` remains a coverage fact. | Changed document bytes makes provenance `stale`; overlay is withheld. |
| Caller bytes unchanged; dependency, config, or producer changes | Coverage is unchanged. | Each independently makes semantic evidence `possiblyStale`. |
| Missing basis | Semantic artifact is malformed and atomically rejected; absence may be exposed as failed/missing coverage. | It cannot be called fresh. |
| Unchanged caller, changed target bytes | Caller coverage is unchanged. | The captured r1 proof cannot itself bind r2 occurrences. With the publication-rejoin rule below, a **new** r2 proof with verified `derivedFrom` may bind exact r2 A anchors and a unique measured same-ID r2 B target; it is `possiblyStale`, `staleTarget=false`, and cannot expand. Without unique B verification, omit only the dependent A fact and mark A partial. |
| Refresh fails | New tuple row is `failed`, diagnostic required. | Old basis remains historical with its old label; never promoted. Only provenance of declaration-keyed facts for returned declarations may appear in the new answer, plus the latest eligible earlier coverage row for each returned declaration's document even without a matching fact ([scope](#historical-evidence-scope)); no old call binding does. |

## Publication-time rejoin of captured SCIP evidence (#11A)

[Decision 0002](decisions/0002-publication-rejoin.md) fixes the closed producer-only `CapturedScipFactV1` envelope and all four v1 shape choices; [acceptance vectors](publication-rejoin-vectors-v1.md) give exact authenticated binary, canonical bytes and negative cases. This is a prospective documentation rule, not permission to reinterpret legacy #26 `SemanticCapture` `formatVersion:1` facts or modify #38 native records. Future #27–#30 use only the new envelope. `derivedFrom.recordKind` names the **raw** `occurrence|symbolInformation|relationship`, never a destination record family. Envelopes and proof tuples are document-only with concrete path/language; `Index.external_symbols` and nested relationships are recursively validated context protected by the binary artifact hash, omitted from the envelope/proof inventory, and cannot claim a document-local anchor. Unknown keys, unknown protobuf fields/semantic extensions, unverified artifacts, duplicate document paths or malformed positions reject atomically.

Before any join, derive each immutable envelope `E` from authenticated SCIP binary with the pinned `scip 0.10.0` decoder; a newly captured JSON producer must use the identical closed shape/digest under an authenticated reproducible raw-field mapping, never reinterpret legacy #26 bytes. `factDigest = SHA-256(UTF8("trellis.semantic-fact.v1\0") || canonicalBytes(E))` is a full lowercase 64-hex hash under [#22 canonical bytes](#canonical-bytes), never the hash of a normalized destination record. Preserve original binary bytes/hash, authenticated manifest, pinned decoder and reproducible envelope inventory. Hash source bytes and binary artifact with empty domain. Deduplicate identical envelopes within one authenticated tuple **before** assigning claims or doing unique proof lookup. `capturedProofId = SHA-256(UTF8("trellis.capture-proof.v1\0") || canonicalBytes({producerId,artifactHash,sourceSetId,sourceRevision,language,relativePath,factDigest}))`; every tuple names its authenticated containing document with concrete `language` and `relativePath`. Raw `Document.position_encoding=0` remains `0` in `E` and its digest; coordinate conversion uses the authenticated manifest encoding pinned to this producer version, and fails only if neither artifact nor manifest supplies valid 1/2/3 (or they conflict). Nonempty `Signature.occurrences` are validated as signature-text presentation context, omitted from the envelope digest, protected by original artifact hash, and never treated as source occurrences. There is no ordinal or family discriminator. A minted `derivedFrom` must resolve its proof ID, raw kind and digest to exactly one deduplicated authenticated captured envelope; full digest equality alone does not authenticate a join. Reject a missing, contradictory, multiply distinct or forged proof. One occurrence may witness multiple downstream records with the **same** raw proof; independent relationships stay distinct. Captured provenance, including native syntax, has `derivedFrom:null`.

Native publication validates captured artifact/basis and source admission, then source positions and measured joins, inside one publication transaction. An independently invoked later importer instead CASes the already published revision; a losing CAS leaves no partial overlay. A malformed captured producer artifact fails **all** its facts/proofs. If another revision publishes, every requested attributable rejected-producer coverage tuple is selected `failed` with diagnostic and zero facts; cancellation publishes nothing. Valid destination A requires identical authenticated source bytes/key and exactly one distinct compatible native r2 source candidate at the converted exact span/kind. Two raw facts matching one measured candidate do not cause anchor ambiguity. Missing/ambiguous A source candidate or unavailable destination measurement fails the entire selected A tuple; no name/enclosing/overlap fallback. No r1 provenance or binding is attached to r2: mint **new** r2 provenance and associations with the r2 native occurrence ID/owner/range, destination document hash and r2 internal target revision. Under [Decision 0003](decisions/0003-per-document-occurrence-ids.md) the r2 occurrence ID of an unchanged-byte document under the same native producer and extraction context equals its r1 ID; rejoin still requires a separately verified r2 native candidate, mints new r2 provenance and associations with the r2 target revision, and that equality is never a validity proof. The captured `SemanticBasis(revisionId=r1)` remains unchanged; a verified `derivedFrom` is its **only** r2 provenance self-match exception. Valid r2 facts with r1 basis are `possiblyStale` under freshness rule 2, cannot expand even with direct dispatch, and keep `staleTarget=false` for a verified internal target; no `staleTarget=true` example or warning is admitted.

A changed B document does not itself rejoin. A dependent A fact may retarget B only if B's r2 native measurements contain **exactly one** compatible declaration whose stable syntax ID equals the captured internal target's verified ID; merely spelling the old ID or guessing by name is not verification. If missing or nonunique, omit only facts in A that depend on B, retain independent A facts, emit A `partial` with true `observedRoles` and diagnostic; never invent external/dangling fallback. Genuine producer-external targets stay external. #47 latest-eligible prior `complete|partial` declaration history remains limited to returned declarations, the same producer/document and verified stable ID after r2 `failed|omitted`; no old occurrence associations or old calls are applied. Changed source bytes yield `stale` **historical** provenance only. #52 revision covers native extraction and source/toolchain/config/dependency, not semantic artifact/producer. #52 warnings-v1 stays unchanged: selected failed/partial yields `coverageIncomplete`; any returned possibly-stale evidence yields `(staleEvidence,null)`, stale history yields `(staleEvidence,provenanceId)`, and omitted alone is not incomplete. No `staleTarget` warning in v1. #40 traversal and #41 integration are separate gates.

## Records and bindings

[Decision 0011](decisions/0011-test-production-declaration-role.md) requires `Declaration.role` on every measured native and normalized declaration. The producer classifies a declaration as test if its #167 test-convention path, a supported syntax marker, or an enclosing test declaration makes it test; otherwise it is production. Rust `cfg(test)` and a direct `test` operand in `cfg(all(test, …))` mark test; `cfg(any(test, …))`, `cfg(not(test))`, and `cfg(feature = "test-support")` do not mark test by themselves. Rust `#[test]` and path-qualified final `::test` attributes mark test. Java JUnit/TestNG test-method annotations and Python `test_*` functions or `Test*` classes in test-convention files mark test; JavaScript/TypeScript use paths only. Classification is not part of identity. The required lowercase values `test` and `production` are unrelated to the evidence-role `Role` enum above. A producer-version change rebuilds the disposable index without migration.

| Record.field | Type / nullability | Invariant | Failure or disposition |
|---|---|---|---|
| `Declaration.syntaxId` | `SyntaxId` | Equals descriptor algorithm. | Digest mismatch invalidates artifact. |
| `.document` | `DocumentKey` | Containing admitted document. | Dangling/mismatch invalidates artifact. |
| `.revisionId` | `Text` | Containing revision. | Mismatch invalidates artifact. |
| `.kind` | `Kind` | Agrees with key/header. | Disagreement invalidates declaration. |
| `.name` | `Text?` | Named declarations non-null; anonymous/module null. | Wrong nullability invalid. |
| `.lookupKey` | `Text?` | Non-null exactly when name is non-null, derived by language rule. | Wrong derivation/nullability invalid. |
| `.role` | `test \| production` | Required measured declaration classification under [Decision 0011](decisions/0011-test-production-declaration-role.md); distinct from the evidence `Role` enum. It is outside the key, `syntaxId` and `lookupKey`. | Missing or unknown role invalidates the declaration. |
| `.ancestors` | `Key[]` | Outermost to immediate parent. | Wrong order/key invalidates ID. |
| `.key` | `Key` | Current declaration key. | Inconsistent descriptor invalid. |
| `.range` | `Range` | Contains nameRange; only empty module may be empty. | Invalid range rejects artifact. |
| `.nameRange` | `Range?` | Named declarations have contained range; anonymous/module null. | Wrong cardinality/range rejects artifact. |
| `.header` | `Header` | Kind/name agree; measured projection only. | Disagreement rejects record. |
| `.provenanceId` | `Text` | Native measured-syntax provenance. | Dangling/wrong kind invalidates artifact. |
| `SymbolKey.scheme` | `scip` | Fixed v1 scheme. | Other scheme invalid. |
| `.symbol` | `Text` | Opaque nonempty semantic spelling. | Empty invalid. |
| `.scope` | `global \| document` | Controls document field. | Other value invalid. |
| `.document` | `DocumentKey?` | Required for document scope; null for global. | Wrong combination invalid. |
| `Symbol.key` | `SymbolKey` | Unique symbol key in producer artifact. | Conflicting duplicate invalidates artifact. |
| `.displayName` | `Text?` | Display only; never identity/lookup proof. | Invalid text rejects record. |
| `.declarations` | `Target[]` | Unique targets; empty means external/unlocated, never local evidence. | Duplicate target invalid. |
| `.provenanceId` | `Text` | Resolves to semantic provenance. | Dangling invalidates artifact. |
| `Target.kind` | `internal \| external` | Selects exactly one variant. | Mixed/incomplete variant invalid. |
| internal `.syntaxId` | `SyntaxId` | In the answer source set, at the revision of the containing fact or binding (the answer snapshot for current facts). | Dangling or other-source target cannot be internal. |
| internal `.document` | `DocumentKey` | Matches syntax declaration. | Mismatch invalid. |
| internal `.revisionId` | `Text` | Equals the revision of the fact or binding that contains it (v1); matches answer snapshot when current. | Any other revision is invalid. A historical fact's target keeps that fact's revision, labelled with derived freshness, never promoted. |
| external `.symbol` | `SymbolKey` | Explicit external boundary, including other source sets in v1. | Missing key invalid. |
| `DeclarationBinding.syntaxId` | `SyntaxId?` | Exact declaration-name join has its one ID; non-exact is null. | Wrong join/cardinality invalid. |
| `.symbols` | `SymbolKey[]` | Exact binding has at least one explicit unique symbol; non-exact diagnostic evidence may be empty. | Exact empty list invalid. |
| `.join` | `Join` | Declaration-name candidate family. | Incompatible join invalid. |
| `.provenanceId` | `Text` | Producer-specific semantic evidence. | Producers are never silently merged. |
| `TypeRelationship.kind` | `extends \| implements \| overrides` | Actual supported relationship. | Name match cannot create it. |
| `.source` | `Target` | Internal; direction subtype→base, implementing type→interface/trait, overriding→base member. | External source/direction error invalid. |
| `.target` | `Target` | Supported target. | Dangling internal target invalid. |
| `.provenanceId` | `Text` | Independent semantic proof. | Missing proof excludes record. |

A syntax ID and a semantic symbol remain distinct. Never substitute a symbol spelling for a syntax ID. Type relationships are not call edges.

## Measured anchor joins

| Record.field | Type / nullability | Invariant | Failure or disposition |
|---|---|---|---|
| `MeasuredAnchor.document` | `DocumentKey` | Exact evidence document. | Mismatch invalid. |
| `.revisionId` | `Text` | Exact evidence revision. | Mismatch invalid. |
| `.contentHash` | `Hash` | Exact bytes used for coordinate conversion. | Mismatch is stale/invalid, never fuzzy join. |
| `.range` | `Range` | Converted UTF-8 scalar-boundary span. | Invalid conversion rejects artifact. |
| `.kind` | `declarationName \| callee \| invocation \| reference` | Selects compatible measured candidate family. | Incompatible family invalid. |
| `Join.anchor` | `MeasuredAnchor` | Exact tuple and span. | No enclosing/name/overlap fallback. |
| `.status` | `exact \| ambiguous \| unmatched \| unsupported` | Cardinality below. | Impossible combination invalid. |
| `.candidateIds` | `(SyntaxId\|OccurrenceId)[]` | Sorted unique; exact=1, ambiguous≥2, unmatched/unsupported=0; family matches kind. | Wrong cardinality/order/family invalid. |
| `.diagnostic` | `Text?` | Null allowed only for exact; required for every non-exact join. | Wrong nullability invalid. |
| `Call.id` | `OccurrenceId` | Derived occurrence identity. | Mismatch invalid. |
| `.ownerSyntaxId` | `SyntaxId` | Owning measured callable/module. | Dangling owner invalid. |
| `.ordinal` | `UInt` | Document-version-local call ordinal under owner. | Duplicate/out-of-order identity invalid. |
| `.document` | `DocumentKey` | Owner document. | Mismatch invalid. |
| `.revisionId` | `Text` | Containing (published or answered) revision; not part of occurrence identity. | Mismatch invalid. |
| `.range` | `Range` | Measured invocation. | Invalid/duplicate native occurrence rejects artifact. |
| `.calleeRange` | `Range?` | When present, lies inside invocation. | Outside range invalid. |
| `.spelling` | `Text?` | Measured spelling; null is allowed. | Never infer spelling. |
| `.regionIds` | `OccurrenceId[]` | Unique containing control regions in adapter-defined containment order. | Dangling/wrong owner invalid. |
| `.provenanceId` | `Text` | Native measured syntax. | Wrong provenance invalid. |
| `ControlRegion.id` | `OccurrenceId` | Control-kind occurrence identity. | Mismatch invalid. |
| `.ownerSyntaxId` | `SyntaxId` | Same owner for parent chain. | Mismatch invalid. |
| `.ordinal` | `UInt` | Control ordinal under owner. | Invalid ordering/duplicate invalid. |
| `.document` | `DocumentKey` | Owner document. | Mismatch invalid. |
| `.revisionId` | `Text` | Containing revision; not part of occurrence identity. | Mismatch invalid. |
| `.kind` | `Text` | Actual adapter syntax-kind, not invented runtime construct. | Empty/invented value excluded. |
| `.range` | `Range` | Measured region. | Invalid range rejects artifact. |
| `.parentId` | `OccurrenceId?` | Same owner/document-version parent containing child; no cycle. | Dangling/non-containing/cyclic parent invalid. |
| `.arm` | `Text?` | Actual measured arm label when available. | Never fabricate. |
| `.provenanceId` | `Text` | Native measured syntax. | Mismatch invalid. |
| `Reference.id` | `OccurrenceId` | Reference occurrence identity. | Mismatch invalid. |
| `.ownerSyntaxId` | `SyntaxId` | Owning declaration/module. | Dangling invalid. |
| `.ordinal` | `UInt` | Reference ordinal under owner. | Duplicate/out-of-order invalid. |
| `.document` | `DocumentKey` | Owner document. | Mismatch invalid. |
| `.revisionId` | `Text` | Containing revision; not part of occurrence identity. Its semantic resolution and targets are valid only at this revision. | Mismatch invalid. |
| `.range` | `Range` | Exact measured reference. | Invalid range rejects artifact. |
| `.spelling` | `Text` | Exact measured identifier spelling. Lookup decoding/normalization affects only `lookupKey`. | Invalid text rejects record. |
| `.lookupKey` | `Text` | Language-specific lookup transform. | Wrong transform invalid. |
| `.site` | `declaration \| use` | Actual site. | Mismatch with role invalid. |
| `.roles` | `Role[]` | Nonempty unique, enum ordered, applicable. | Bad role set invalid. |
| `.resolution` | `Resolution` | Cardinality below. | Impossible combination invalid. |
| `.declaredTarget` | `Target?` | Resolution cardinality below. | Impossible combination invalid. |
| `.candidates` | `Target[]` | Distinct sorted candidates per resolution. | Wrong count/order invalid. |
| `.provenanceId` | `Text` | Semantic producer evidence. | Dangling invalid. |

Exact joins require exact source set, document, revision, content hash and span with one compatible measured candidate. UTF-16 or scalar offsets are first converted against the exact source. An enclosing definition, same name, overlapping span, or reference role is insufficient. Ambiguous, unmatched, and unsupported evidence remains explicit.


Join checks are concrete and count distinct compatible measured candidate IDs, not producer facts. A declaration-name anchor on exact bytes whose span equals only measured declaration `S1` is `{status:exact,candidateIds:[S1],diagnostic:null}` and can install a declaration binding. Two producer facts at that anchor that both map to `S1` still yield the same exact join with `[S1]`; their separate provenance is preserved. If the exact anchor instead matches distinct compatible measured declarations `S1` and `S2`, the join is `{status:ambiguous,candidateIds:[S1,S2],diagnostic:"two distinct compatible measured declarations"}` and installs no binding. No compatible measured candidate gives `unmatched` with `[]` and a diagnostic; an unavailable position convention gives `unsupported` with `[]` and a diagnostic. A UTF-16 offset that splits a surrogate pair cannot be converted to a scalar-boundary UTF-8 range: it is an invalid location and atomically rejects the artifact, not an ambiguous or unsupported join.

Join cardinality and semantic resolution are separate. After the one-candidate exact join to `S1`, compatible facts from the selected producer may agree on one semantic target `T1`, yielding a resolved binding to `T1`. If preserved facts with distinct provenances instead assert contradictory exact semantic targets `T1` and `T2`, the measured join remains `{status:exact,candidateIds:[S1],diagnostic:null}`, while resolution becomes `{resolution:ambiguous,declaredTarget:null,candidates:[T1,T2]}`. It is the contradictory target binding, not the number of producer records, that makes resolution ambiguous.

## #279 amendment: native identifier-site candidates

**Status: ratified by the owner, 2026-10-24.** This section defines native-v9's reference inventory, ownership and narrow import-alias declarations. It distinguishes native measured candidates from the semantic `Reference` record above; it adds no semantic roles, resolution, targets, dispatch or invocation merely because an identifier exists.

A native reference candidate carries `{id,ownerSyntaxId,ordinal,document,revisionId,range,spelling,lookupKey,provenanceId}`. These use the existing occurrence, syntax, document, revision, range and lookup types. Spelling is the exact identifier source bytes; provenance is native `measuredSyntax` with no semantic basis. The native producer measures these candidates from its already parsed tree, outside opaque syntax, without a second parser in the capture adapter, whole-source name scanning or reconstruction from SCIP.

| Language | Admitted parser-proved identifier leaf kinds and sites |
|---|---|
| Java | `identifier`, `type_identifier`: declaration names, value/member uses, callees, types and dotted import names, including the final identifier of `this::target` / `Type::target`. |
| Rust | `identifier`, `type_identifier`, `field_identifier`: declaration names, value/member uses, callees, paths, types and use-tree names, including explicit import aliases. |
| Python | `identifier`: declaration/binding names, value/attribute uses, callees, dotted imports, aliases and annotations. |
| JavaScript | `identifier`, `property_identifier`, `shorthand_property_identifier`, `shorthand_property_identifier_pattern`: declaration/binding names, value/member uses, callees, import/export identifier specifiers and aliases, and class/type uses already valid JavaScript; no TypeScript syntax. |

A candidate describes a lexical site, not a read or definition. Exclude comments, string/template literal text and escape subnodes, regex text, Rust lifetimes/labels/metavariables and opaque macro/attribute token-tree contents, JavaScript private identifiers and keyword property tokens. Visit parsed interpolation expressions normally. A kind outside this list is neither a candidate nor a gap merely because it exists. Semantic sites and roles still require their existing exact joins and authenticated producer proof.

**Ownership.** At an exact measured declaration name, the candidate has that declaration-name anchor's owner: its lexical parent, or itself when no parent exists (`parentRef ?? self`). All other candidates inherit the existing call/control executable-container owner at that source site. A nested callable owns body/header use sites, but its own name belongs to its lexical parent. A parameter name belongs to its measured declaration's parent. Variable/parameter declarations do not take ownership of initializer calls or ordinary uses; implementing references must not change call/control ownership. A semantic declaration-site reference uses this same declaration-name anchor owner, not an independently chosen owner equal to the named declaration.

**Identity and semantic subsets.** Deduplicate owner/range candidates during native collection, order by `(start,end)` independently for each document/owner and assign contiguous `kind:"reference"` ordinals before semantic producer joins. Published duplicate native owner/kind/range rows are still invalid. Use existing `occ:v2`; native-v9 is a producer-version change, not a new identity domain. Semantic references must be exact members of this full native inventory and retain its IDs and ordinals even if one producer reports only a later candidate or different producers report different subsets. Never renumber a semantic subset or infer identity from producer record order.

**Narrow alias declarations.** Add measured `kind:"alias"` declarations only for JavaScript `import {original as local} from "module"` with identifier `original` and identifier `local`, and Rust `use path::original as local`, including a `use path::{original as local}` leaf. Name and name range are exactly `local`; declaration range is the containing import-specifier / use-as clause; parent is the existing lexical declaration/module; no signature is added. Keep imported-name and alias identifier candidates. The `local` reference candidate belongs to the declaration's parent, matching its declaration-name anchor. No alias role or target is inferred. JavaScript default/namespace imports, string-named imports and re-export aliases, and Rust glob imports / `as _`, gain no alias declaration under this rule. Other admitted identifiers there remain lexical candidates. Python alias candidates are allowed, but no new Python alias-declaration family or exact alias anchor is claimed.

Other binding sites without a measured declaration remain candidates, but cannot acquire `definition` or `alias` from a guessed declaration-name join. The corpus alias role stays required for JavaScript and Rust, exempt for Java and Python under [Decision 0017 Amendment 1](decisions/0017-conformance-corpus-storage-and-producers.md#amendment-1-2026-10-23-outcome-floors). Python's applicable language role is not proof that its pinned producer supplies an exact alias anchor. No reference or alias floor is changed.

## Owner-ratified native-v9 correction: Java varargs

**Status: owner-ratified 2026-10-24 (#279).** This narrow Java measurement ruling is approved independently; the #279 capability and reference amendments above are also ratified by the owner, 2026-10-24.

A Java `spread_parameter` produces exactly one measured `kind:"parameter"` declaration, with its actual name and type measured from the parser's spread-parameter structure. Its lexical parent is the containing method or constructor. Do not admit a `variable_declarator` whose direct parent is that `spread_parameter` as a separate variable declaration. Continue traversing the nested syntax so the parameter's identifier remains a native reference candidate; its declaration-name owner is the method or constructor, not a synthetic nested variable.

This deliberately replaces native-v8's nested variable plus nameless parameter with one correctly named and typed parameter in native-v9. Use the same measured parameter parts for signature, header and witnesses, preserve the variadic flag and literal type bytes, and do not fabricate a type or append synthetic `...` to its witness. The change does not alter ordinary variable declarations or call/control ownership, infer semantic roles or targets, change an identity domain or change corpus floors. Any resulting declaration/ancestor identity changes must be explained as this measurement correction, separately from native producer-version occurrence-ID changes.

## Lookup and reference roles

Stable identity always preserves measured name bytes. Lookup first uses the language's lexically decoded identifier (for example, remove Rust `r#` or decode Java/JavaScript identifier escapes), then applies:

| Language | Lookup rule | Required check |
|---|---|---|
| Java | Exact code points; no normalization or case folding. | `é` (U+00E9) and `e`+U+0301 have distinct lookup keys and distinct IDs. |
| Rust | NFC. | The two spellings share a lookup key but retain distinct measured names and IDs. |
| Python | NFKC. | The two spellings share a key; fullwidth `Ａ` maps to lookup `A`; all measured spellings still yield distinct IDs. |
| JavaScript | Exact code points; no normalization or case folding. | NFC/NFD spellings remain distinct keys and IDs. |

Resolution and lookup dependencies use lookup keys; every hash input uses measured spelling, never normalized lookup text. A normalization collision remains ambiguous without language scope/shadowing proof.

| Role | Java | Rust | Python | JavaScript | Meaning |
|---|---:|---:|---:|---:|---|
| definition | yes | yes | yes | yes | A declaration occurrence. |
| read | yes | yes | yes | yes | A value read. |
| write | yes | yes | yes | yes | A value assignment/write. |
| call | yes | yes | yes | yes | Callee use of a measured invocation, not a passed callback/method reference. |
| type | yes | yes | yes | yes | Use as type/class/trait, including heritage and JS class/`instanceof`; no fabricated JS type annotation or TypeScript. |
| import | yes | yes | yes | yes | Actual imported-name use. |
| alias | **not applicable** | yes | yes | yes | Actual alias declaration, accompanied by definition; never an ordinary use. |

Java import names are import uses, not aliases. Rust `use … as …`, Python import aliases, and JavaScript renamed imports are legitimate aliases. Roles may combine only when one occurrence supports each role and remain enum ordered. Each `Reference` record counts once, not once per role, toward the later corpus floor of 40 reference facts **per language**. Every applicable role must occur in that later corpus. This contract adds no corpus facts and permits no further omission without a separate owner-approved decision.

## Dispatch

Resolution has exact cardinality:

| Resolution | `declaredTarget` | `candidates` |
|---|---|---|
| `resolved` | one internal target | empty |
| `external` | one external target | empty |
| `ambiguous` | null | at least two distinct sorted targets |
| `unresolved` | null | empty |

Duplicate contradictory exact bindings from the selected producer become ambiguous; first-wins is forbidden and both provenances remain.

| `CallBinding` field | Type / nullability | Invariant | Failure or disposition |
|---|---|---|---|
| `callId` | `OccurrenceId?` | Exact compatible join gives one call; non-exact is null. | Wrong combination invalid. |
| `join` | `Join` | Exact callee span, or invocation span only when uniquely matched. | Non-exact cannot enrich a call. |
| `resolution` | `Resolution` | Cardinality table applies. | Impossible shape invalid. |
| `declaredTarget` | `Target?` | Cardinality table applies. | Impossible shape invalid. |
| `candidates` | `Target[]` | Cardinality table applies. | Wrong order/count invalid. |
| `dispatch` | `direct \| constructor \| virtual \| interface \| dynamic \| unknown` | Producer-supported static property. | A declaration binding alone cannot establish it. |
| `possibleDispatch` | `Target[]` | Unique evidence only, never executable edges or exhaustive runtime set. | Never authorizes expansion. |
| `possibleDispatchComplete` | literal `false` | v1 never claims completeness. | `true` is invalid. |
| `staleTarget` | `boolean?` | Null with no internal target; false for a uniquely verified measured same-revision internal target; positive true remains invalid in v1. | Wrong derivation invalid. |
| `provenanceId` | `Text` | Selected producer evidence. | Dangling/mismatched provenance invalid. |

Expansion is allowed **only** for a fresh, exact, unambiguous internal call binding with `dispatch=direct|constructor` and `staleTarget=false`. Direct means a producer-established statically selected body (named function/static method, or a language-specific fixed method/constructor). Virtual/interface/dynamic/unknown remains a boundary even with one possible target. External remains external. A reference never creates a call. A callback value or method reference is not a call to its body; an actual scheduler invocation calls only the scheduler. Possible dispatch is evidence, not execution.

### Advisory native candidates for the first MCP call view (owner decision 2026-09-25)

The first-release `outgoing_calls` **wire projection only** may show bounded `nativeCandidates` as advisory lexical name-match hints. This adds no field to any authored or normalized v1 record, `GraphRequest`, `GraphResult`, `GraphEdge`, warning, ID vector, corpus fixture or checker. #24 owns the closed response shape, cap, ordering and clipping; #17 implements that projection. An entry is eligible only when the measured `calleeRange` and full non-null `Call.spelling` are verified to denote one language-decoded identifier token (not a compound expression) and an already indexed, fresh native `Declaration` in the same eligible response snapshot (whether or not optional `expectedBasis` was supplied), language and source set has an equal language-specific decoded `lookupKey`. The displayed `syntaxId` must be the declaration's verified canonical ID from that snapshot, **never** an ID guessed from a name, overlapping span, legacy node ID or SCIP symbol. No receiver, scope, import, overload or dispatch resolution follows from this lexical match. If a callee token, declaration ID, eligible snapshot, native provenance or coverage cannot be checked, omit that hint; an empty list does not prove that there is no callee.

These hints are not `DeclarationBinding`, `CallBinding`, `Target`, `possibleDispatch`, resolution, exact join, call edge or completeness evidence. A hint never sets `GraphEdge.to`, `CallItem.targetId` or a resolved disposition, and never authorizes graph traversal. The underlying call and shown declaration each retain their own native provenance and coverage; the *name-match relation itself* has no producer provenance and must not borrow the call's provenance as semantic proof. The projection may include both source facts' provenance/coverage but must not invent a hint-specific producer fact. Candidate presence, absence or clipping does not create any warning outside the exact warnings-v1 rules. `nativeCandidates` is always explicitly non-exhaustive as a possible-callee universe, even if every currently indexed lexical match fits. #40 still owns outgoing graph traversal using only admitted exact semantic bindings, not these hints. No change to #26's in-flight authored corpus schemas, fixtures, counts or checker is required.

**Owner amendment (2026-10-17, #167; [Decision 0011](decisions/0011-test-production-declaration-role.md)):** for a selected production declaration, a matching test declaration is not eligible. Use the stored caller and candidate `Declaration.role`, not their paths, to decide eligibility; this includes inline Rust tests. #24's ordering ranks hints nearer the calling document first. Neither change makes a hint evidence of anything.

For member invocation `obj.foo()`, the measured member-name `foo` can be this single identifier only if `Call.calleeRange` spans exactly that source token and full `Call.spelling` is `foo`. Captured compound expression `obj.foo` must not be split or suffix-matched into a hint; missing token proof yields no hint. #38 owns source-adapter token capture and tests; this clarification does not alter #26's active authored corpus contract. The existing owner ruling of 2026-10-23 (#194 item 3 option (a)) also admits a Rust `scoped_identifier` callee's parser `name` field when it is a lone `identifier`, after unwrapping `generic_function.function` where present. `Self::foo()`, `module::foo()`, `Type::foo()`, `Type::<T>::foo()` and `Type::foo::<T>()` can therefore measure exactly `foo`; a non-identifier final segment supplies no verified token. This is still lexical name matching, never path resolution or text suffix splitting. [Decision 0012](decisions/0012-incoming-calls-syntax-tier.md) and #24 apply it to incoming matching and outgoing hints with unchanged wire shapes and limitations; the measured-field change ships under native-v9.


## Canonical bytes

Canonical JSON uses documented ASCII object keys sorted by ASCII byte value. Arrays retain their specified order unless explicitly a sorted set. Required nulls remain. Valid scalar strings are UTF-8 without normalization. Escape U+0022 quote as `\"`, U+005C backslash as `\\`, and **every** U+0000–U+001F control as six ASCII bytes `\u00xx` with lowercase hex digits. Do not use short control escapes (`\b`, `\t`, `\n`, `\f`, `\r`) or `\u0022`/`\u005c`; U+2028 and U+2029 remain literal UTF-8. Booleans/null use JSON literals and integers use minimal unsigned decimal. There is no whitespace, BOM, trailing newline, duplicate key, floating point, or negative zero. This is deliberately not RFC 8785.

Every separated digest is `SHA-256(UTF8(domain) || canonicalInputBytes)`, with no extra delimiter. Each named domain includes its terminating NUL byte. Output is lowercase hex. Source-content, manifest-component, executable/artifact/tool/config/dependency hashes described as having an empty prefix hash their exact declared bytes without a domain.

## Stable identifiers

| Nested field | Type / nullability | Invariant / failure |
|---|---|---|
| `Signature.parameterTypes` | `Text[]` | Exact measured Java parameter-type spellings in order; invalid text rejects. |
| `.typeParameterCount` | `UInt` | Measured Java callable generic count. |
| `.variadic` | `boolean` | True exactly for final varargs form. |
| `Key.kind` | `Kind` | Measured declaration kind. |
| `.name` | `Text?` | Exact measured spelling or null for anonymous/module. |
| `.signature` | `Signature?` | Non-null only for Java method/constructor; null for Rust/Python/JavaScript. |
| `.ordinal` | `UInt` | Zero-based position in exact sibling group; singleton is zero. |

Java signature excludes return types and parameter names. Java ordinary functions do not exist. Python overload-decorated declarations are repeated syntax, not Java overload identities. Rust and JavaScript have no overload discriminator in v1.

Group siblings by immediate container, kind, **exact measured name**, and canonical signature (including null). Sort them by `(start,end)` and assign zero-based ordinals. Equal ranges in one group are invalid rather than tie-broken. Anonymous declarations group with null name by the same rule.

The stable input is exactly:

```text
{sourceSet:Text,path:Path,language:Language,ancestors:Key[],declaration:Key}
```

Ancestors run outermost to immediate parent. A top-level declaration has `[]`; a measured module is not redundantly inserted into child ancestors. Header, ranges, revision, file hash, lookup keys, producer, and semantic symbols are excluded. Domain bytes are `trellis.syntax.v1\0`. Calculate the **full 256-bit SHA-256 digest** of the domain and canonical input; emit `sid:v1:` followed by its first 32 lowercase hex characters (first 16 digest bytes). The vector's `syntax.sha256` and all integrity hashes retain the full 64 hex characters. Moving source set/path, rename, container-key change, or ordinal shift can change IDs and therefore descendant IDs. A body-only edit cannot.

Occurrence input is exactly `{contentHash:Hash,extractionContext:Hash,nativeProducerId:Text,nativeProducerVersion:Text,ownerSyntaxId:SyntaxId,kind:call|reference|control,ordinal:UInt}`, domain `trellis.occurrence.v2\0` ([Decision 0003](decisions/0003-per-document-occurrence-ids.md)). `contentHash` is the containing document's exact content digest. `extractionContext` is the full SHA-256 over domain `trellis.extraction-context.v1\0` and canonical `{language,components:[{name,hash}]}`, where `components` lists, sorted by name then hash and unique, **every** non-source input (configuration, toolchain or dependency capture, by captured component digest) that the native producer reads for that language and that can affect any native measured field or projection; `[]` when none. This digest is the extraction-context component of #71's cache key `(language, extractor version, extraction-context digest, content hash)`. **No `occ:v2` record may be minted or published** until the producer version's complete input inventory and every component capture are authenticated against the pinned revision and native producer descriptor; an undeclared, missing, stale or mismatched component, or an unproven `[]`, fails that precondition, so the affected documents get no `occ:v2` records and publication fails closed for them, with retry only after authentication succeeds. Re-measurement cannot repair an unauthenticated inventory. A missing or corrupt optional cache entry whose inputs are validly attested is an ordinary cache miss and may trigger normal measurement. Native measurement is strictly document-local: under `occ:v2` a native producer must not read any other source document to measure one, and cross-document source dependencies are not representable in this version. `nativeProducerId`/`nativeProducerVersion` are the native producer descriptor's `id` and `version`; any extractor change that can alter **any** native measured field or projection, or that starts reading another input, must change that version. Its `ownerSyntaxId` is the emitted 128-bit `sid:v1:` value, not the full syntax digest. Calculate the full SHA-256 digest of these occurrence bytes and emit `occ:v2:` followed by its first 32 lowercase hex characters. For each owner and kind independently, order measured occurrences by `(start,end)`. Collapse duplicate semantic facts onto one measured reference. Duplicate native occurrences with identical owner/kind/range invalidate the artifact. Occurrence IDs are document-version-local, subject to [Decision 0015](decisions/0015-native-hash-based-identity.md)'s accepted full-digest collision assumption: a byte-identical document at the same key, under the same native producer ID, version and extraction context, keeps identical occurrence IDs in every revision; a byte change to one document re-identifies only that document's occurrences, a shared context component change re-identifies every document whose context includes it, and a native producer ID or version change re-identifies every document it measures. Semantic validity remains revision-scoped (see [historical evidence scope](#historical-evidence-scope)). The former revision-bound `occ:v1` derivation is withdrawn. These prose derivations are not extra normative vectors.

The 128-bit handles are not integrity proofs or authentication tokens. **Hash-based identity** for the two native ID namespaces, `sid:v1:` and `occ:v2:`, uses a producer-retained registry keyed by `(ID namespace, first 16 SHA-256 digest bytes)` with the binary 32-byte full SHA-256 digest as its value ([Decision 0015](decisions/0015-native-hash-based-identity.md)). Digest computation still uses the validated canonical input and its existing NUL-terminated domain. On a matching typed handle, a different full digest is a detected collision and rejects the affected artifact atomically; an equal full digest is treated as the same identity, without retaining or comparing canonical input bytes. The owner accepts the nonzero residual risk that different canonical inputs may have the same full SHA-256 digest and thus alias under this rule. Never alias a **detected** collision, add an ad hoc suffix, or fall back to another ID length. Identical canonical syntax inputs intentionally retain one stable ID across revisions. Display keys are presentation only: they are never identity, never accepted as input, and never hashed. The future MCP result shape belongs to #24.

The nested digest inputs are closed as well:

| Input field | Type | Invariant / failure |
|---|---|---|
| stable `.sourceSet` | `Text` | Exact logical source-set ID; invalid text rejects input. |
| stable `.path` | `Path` | Exact preserved relative path. |
| stable `.language` | `Language` | Must match declaration/document. |
| stable `.ancestors` | `Key[]` | Exact outermost→parent keys. |
| stable `.declaration` | `Key` | Exact focused key. |
| occurrence `.contentHash` | `Hash` | Exact content digest of the owner's document version. |
| occurrence `.extractionContext` | `Hash` | Authenticated extraction-context digest of the owner's document (above); the context component of #71's cache key. |
| occurrence `.nativeProducerId` | `Text` | Native producer descriptor `id`. |
| occurrence `.nativeProducerVersion` | `Text` | Native producer descriptor `version`; changes whenever any native measured field or projection can change. |
| occurrence `.ownerSyntaxId` | `SyntaxId` | Existing owner in that document version. |
| occurrence `.kind` | `call \| reference \| control` | Selects its independent ordinal namespace. |
| occurrence `.ordinal` | `UInt` | Zero-based `(start,end)` position in that namespace. |
| source-manifest row `.document` | `DocumentKey` | Existing revision document; rows sort language then path. |
| source-manifest row `.contentHash` | `Hash` | Exact document content digest. |
| sibling-group input `.headers` | `Hash[]` | Every group member's header hash in sibling source order; nonempty. |

Any missing/unknown nested field, mismatch, duplicate manifest document, or wrong specified order invalidates the containing digest/basis artifact.

## Durable anchors

`Header` is exactly `{kind:Kind,name:Text?,modifiers:Text[],typeParameters:Text[],parameters:Parameter[],resultType:Text?,bases:Text[]}` and `Parameter` is exactly `{name:Text?,type:Text?,variadic:boolean}`.

| Nested/record field | Type / nullability | Invariant | Failure/disposition |
|---|---|---|---|
| `Header.kind` | `Kind` | Matches declaration/key. | Mismatch invalid. |
| `.name` | `Text?` | Exact measured name/null rule. | Mismatch invalid. |
| `.modifiers` | `Text[]` | Measured order and spelling. | Do not invent absent syntax. |
| `.typeParameters` | `Text[]` | Measured order/spelling. | Do not semantically normalize. |
| `.parameters` | `Parameter[]` | Measured declaration order. | Missing/reordered row invalid. |
| `.resultType` | `Text?` | Measured spelling or null. | No inferred type. |
| `.bases` | `Text[]` | Measured order/spelling. | No inferred relationship. |
| `Parameter.name` | `Text?` | Measured spelling or null. | No invented name. |
| `.type` | `Text?` | Measured type-designator spelling or null. | No inferred type. |
| `.variadic` | `boolean` | Measured form. | Wrong projection invalid. |
| `DurableAnchor.syntaxId` | `SyntaxId` | Captured exact identity. | Never name-search replacement. |
| `.document` | `DocumentKey` | Captured document. | Mismatch invalid. |
| `.capturedRevisionId` | `Text` | Capture revision. | Dangling invalid. |
| `.headerHash` | `Hash` | Hash of canonical `Header`, domain `trellis.header.v1\0`. | Mismatch invalid. |
| `.siblingGroupHash` | `Hash` | Hash of `{headers:Hash[]}` in sibling source order, domain `trellis.sibling-group.v1\0`. | Mismatch invalid. |
| `.siblingCount` | `UInt` | Positive, exact array length. | Zero/mismatch invalid. |
| `.identicalHeaderCount` | `UInt` | Positive count of captured header hash and ≤ siblingCount. | Impossible count invalid. |
| `GroupContinuity.fromRevisionId` | `Text` | Capture revision. | Mismatch invalid. |
| `.toRevisionId` | `Text` | Candidate revision. | Mismatch invalid. |
| `.state` | `unchanged \| changed \| unknown` | Independent exact group membership/order assertion. | Never infer from equal hashes/counts. |
| `.evidence` | `Text?` | Required for cross-revision `unchanged`; null for `unknown`; may explain `changed`. | Wrong nullability invalid. |
| `AnchorResult.status` | `attached \| orphaned` | Controls target/reason. | Impossible combination invalid. |
| `.targetId` | `SyntaxId?` | Attached equals captured ID; orphaned is null. | Searching/replacement invalid. |
| `.reason` | `none \| missing \| headerMismatch \| groupChanged \| unprovenContinuity` | Attached=`none`; orphaned uses a non-none reason. | Wrong combination invalid. |

Header is an identity-header metadata projection, not a source snippet, token stream, full AST, or semantic equivalence statement. It excludes bodies, trivia, default-expression bodies, and annotation/decorator argument expressions. Equal projected headers are conservatively identical even if unrepresented details differ.

Reattachment order is fixed: missing old ID → `missing`; unequal header hash → `headerMismatch`. When either captured or current identical-header count exceeds one, require equal group hash, equal counts, and independently `unchanged` membership/order. Changed continuity or unequal hash/count → `groupChanged`; unknown cross-revision continuity → `unprovenContinuity`. Same-revision continuity is implicit. Otherwise attach the exact old ID. A unique-header anchor with matching ID/header does not require unchanged group membership.

Worked anchor audit. The following common preconditions and ordered checks apply to every row. `D={sourceSetId:core,language:java,path:src/A.java}` is the anchor's captured `document`; `r1` is its valid `capturedRevisionId`; the candidate snapshot is valid revision `r2`, contains the same `D`, and every listed candidate declaration has `document=D` and `revisionId=r2`. The captured `syntaxId` is `S0`. `H0` is the captured `headerHash`; `Hx≠H0`; `G([...])` is the `siblingGroupHash` over the exact ordered sibling group. A member is shown as `(ID,ordinal,headerHash)`, where ordinal is its zero-based position in that exact group. Each row audits in this fixed order: (1) validate captured `D`/`r1` and candidate `D`/`r2`; (2) test presence of old ID `S0`; (3) identify `S0`'s current focused ordinal and exact sibling group; (4) compare its current header hash with `H0`; (5) compare captured/current group hashes, `siblingCount`, and `identicalHeaderCount` when the duplicate rule applies; (6) inspect `GroupContinuity={fromRevisionId:r1,toRevisionId:r2,state,evidence}` when required; (7) emit the complete `AnchorResult`. A row that terminates earlier still records later audit inputs and says why they are not consulted.

| Scenario | Captured anchor and current audit in the required order |
|---|---|
| Unique body edit | Captured: group `[(S0,0,H0)]`, `siblingGroupHash=G([H0])`, `siblingCount=1`, `identicalHeaderCount=1`. (1) common document/revisions match. (2) `S0` is present. (3) focus is `(S0,0,H0)` in exact current group `[(S0,0,H0)]`. (4) header equals `H0`. (5) current `G([H0]),1,1` equals captured, but both identical counts are one, so the duplicate rule is inactive. (6) continuity is `unknown,evidence=null` and is not required for a unique header. (7) `AnchorResult={status:attached,targetId:S0,reason:none}`. |
| Duplicate-header body edit, proven unchanged | Captured: focus `(S0,0,H0)` in `[(S0,0,H0),(S1,1,H0)]`, hash/counts `G([H0,H0]),2,2`. (1) common document/revisions match. (2) `S0` is present. (3) focus remains ordinal 0 in that exact current group. (4) header equals `H0`. (5) current hash/counts are `G([H0,H0]),2,2`, equal to captured; duplicate rule applies. (6) continuity is `{fromRevisionId:r1,toRevisionId:r2,state:unchanged,evidence:"adapter member/order proof E01"}`. (7) `AnchorResult={status:attached,targetId:S0,reason:none}`. |
| Unique sibling inserted before | Captured: focus `(S0,0,H0)` in `[(S0,0,H0)]`, hash/counts `G([H0]),1,1`. (1) common document/revisions match. (2) old ID `S0` is present, now assigned to the inserted member because its key and ordinal are the old descriptor's. (3) focus is `(S0,0,Hx)` in exact current group `[(S0,0,Hx),(Snew,1,H0)]`; the old source member is ordinal 1 with new ID `Snew`. (4) `Hx≠H0`. (5) current `G([Hx,H0]),2,1`; these and (6) continuity `{fromRevisionId:r1,toRevisionId:r2,state:changed,evidence:"member inserted at ordinal 0"}` are recorded but not consulted because the header check already failed. (7) `AnchorResult={status:orphaned,targetId:null,reason:headerMismatch}`; never follow `Snew`. |
| Different-header sibling inserted after unique member | Captured: focus `(S0,0,H0)` in `[(S0,0,H0)]`, hash/counts `G([H0]),1,1`. (1) common document/revisions match. (2) `S0` is present. (3) focus remains `(S0,0,H0)` in exact current group `[(S0,0,H0),(Sx,1,Hx)]`. (4) header equals `H0`. (5) current `G([H0,Hx]),2,1` differs from captured, but both captured/current identical-header counts for `H0` are one, so the duplicate rule is inactive. (6) continuity is `{fromRevisionId:r1,toRevisionId:r2,state:changed,evidence:"different-header member inserted at ordinal 1"}` and is not required for a unique header. (7) `AnchorResult={status:attached,targetId:S0,reason:none}`. |
| Duplicate count changes | Captured: focus `(S0,0,H0)` in `[(S0,0,H0),(S1,1,H0)]`, hash/counts `G([H0,H0]),2,2`. (1) common document/revisions match. (2) `S0` is present. (3) focus remains ordinal 0 in exact current group `[(S0,0,H0),(S1,1,H0),(S2,2,H0)]`. (4) header equals `H0`. (5) current `G([H0,H0,H0]),3,3` differs from captured; duplicate rule applies and fails. (6) continuity is `{fromRevisionId:r1,toRevisionId:r2,state:changed,evidence:"third identical-header member inserted at ordinal 2"}`. (7) `AnchorResult={status:orphaned,targetId:null,reason:groupChanged}`. |
| Same-count reorder/replacement, continuity unknown | Captured: focus `(S0,0,H0)` in `[(S0,0,H0),(S1,1,H0)]`, hash/counts `G([H0,H0]),2,2`. (1) common document/revisions match. (2) `S0` is present. (3) focus is `(S0,0,H0)` in exact current projected group `[(S0,0,H0),(S1,1,H0)]`; projected bytes alone cannot establish that these are the same physical members in the same order. (4) header equals `H0`. (5) current `G([H0,H0]),2,2` equals captured; duplicate rule applies. (6) continuity is `{fromRevisionId:r1,toRevisionId:r2,state:unknown,evidence:null}`. (7) `AnchorResult={status:orphaned,targetId:null,reason:unprovenContinuity}`. |
A body-only or unrelated edit therefore preserves duplicate anchors only with independent unchanged continuity. Insertion/removal affecting duplicates and unprovable same-count replacement/reorder orphan. There is no promise to detect physical replacement by an indistinguishable descriptor. The later vector supplies a continuity assumption; #10 and later corpora own classifier proof.

## Rooted graph answers

| Record.field | Type / nullability | Invariant | Failure/disposition |
|---|---|---|---|
| `GraphRequest.sourceSetId` | `Text` | Admitted source set. | Denied/unknown is `sourceSetDenied`. |
| `.revisionId` | `Text` | Exact immutable revision; no current fallback. | Missing is `revisionUnavailable`. |
| `.rootSyntaxId` | `SyntaxId` | Exact root, no fuzzy name. | Missing is `rootMissing`. |
| `.semanticProducerId` | `Text?` | Null means syntax-only; otherwise exactly one producer, no merge. | Unavailable is `producerUnavailable`. |
| `.depth` | `UInt` | Default 2; valid 0–5; no clamping. | Outside range is `invalidRequest`. |
| `.maxNodes` | `UInt` | Default 150; valid 1–150; root counts. | Outside range is `invalidRequest`. |
| `.maxCalls` | `UInt` | Default 500; valid 0–500. | Outside range is `invalidRequest`. |
| `GraphNode.declaration` | `Declaration` | In pinned snapshot/source set. | Dangling node invalidates result. |
| `.depth` | `UInt` | BFS distance from root. | Incorrect distance invalidates result. |
| `GraphEdge.call` | `Call` | One edge per measured call. | Never one per possible dispatch target. |
| `.from` | `SyntaxId` | Call owner/admitted node. | Mismatch invalid. |
| `.to` | `SyntaxId?` | Non-null only for admitted node. | Refused/boundary target stays only in binding. |
| `.binding` | `CallBinding?` | Selected producer enrichment. | Missing is explicit boundary. |
| `.visit` | `new \| seen \| boundary` | `new/seen` only with admitted target; boundary otherwise. | Inconsistent shape invalid. |
| `.boundaryReason` | `none \| external \| unresolved \| ambiguous \| dispatch \| stale \| nodeLimit \| missingEvidence` | `none` only for new/seen; precedence below. | Inconsistent reason invalid. |
| `Frontier.reason` | `depth \| nodeLimit \| callLimit` | Budget/depth only. | Semantic boundaries are edges, not frontiers. |
| `.nodeId` | `SyntaxId` | Source/queued node. | Dangling invalid. |
| `.callId` | `OccurrenceId?` | NodeLimit uses refused emitted call; depth/callLimit null. | Wrong combination invalid. |
| `.targetId` | `SyntaxId?` | NodeLimit uses refused target; otherwise null. | Wrong combination invalid. |
| `.nextOrdinal` | `UInt?` | Depth=0; callLimit=next local call ordinal; nodeLimit=null. | Wrong combination invalid. |
| `.omittedCalls` | `UInt` | Depth/callLimit exact remaining count; nodeLimit 0. | Wrong arithmetic invalid. |
| `Warning.code` | `coverageIncomplete \| staleEvidence \| staleTarget \| bindingAmbiguous \| syntaxOnly` | Required exactly as in Warning completeness (warnings-v1). | Unknown code invalid. |
| `.message` | `Text` | Concrete warning. | Empty invalid. |
| `.provenanceId` | `Text?` | Relevant returned provenance, or null for an aggregate warning (including `possiblyStale`). | Dangling invalid. |
| `GraphResult.request` | `GraphRequest` | Exact effective request, with defaults materialized. | Mismatch invalid. |
| `.resolvedRevisionId` | `Text` | Equals pinned revision. | Fallback/mismatch invalid. |
| `.nodes` | `GraphNode[]` | Admission order. | Wrong order/duplicate invalid. |
| `.edges` | `GraphEdge[]` | Dequeue then local call order. | Wrong order/duplicate invalid. |
| `.frontier` | `Frontier[]` | Creation order; queued stop rows in queue order. | Wrong order/arithmetic invalid. |
| `.coverage` | `Coverage[]` | Selected/native evidence relevant to nodes/calls, including explicit missing diagnostics. | Hidden absence invalid. |
| `.provenance` | `Provenance[]` | Relevant evidence and freshness. | Dangling/omitted selected evidence invalid. |
| `.partial` | `boolean` | True iff truncated, selected coverage not complete, or any boundary edge. | False never asserts runtime completeness. |
| `.truncated` | `boolean` | True iff any depth/node/call frontier exists. | Exact cap alone does not set it. |
| `.warnings` | `Warning[]` | Exactly the warnings-v1 set; unique by `(code,provenanceId)`; sorted by `(code,provenanceId,message)`. | Missing, extra, duplicate-key, or unsorted warning invalid. |
| `Error.code` | `invalidRequest \| invalidRecord \| invalidRange \| invalidDigest \| unsupportedEncoding \| sourceSetDenied \| revisionUnavailable \| rootMissing \| producerUnavailable` | Exact failure category. | Unknown code invalid. |
| `.message` | `Text` | Concrete failure. | Empty invalid. |
| `.field` | `Text?` | Offending logical field when applicable. | No fabricated field. |
| success `.ok` | literal `true` | Selects success variant. | Other/mixed variant invalid. |
| success `.result` | `GraphResult` | Present only on success. | Missing result or any error field invalid. |
| failure `.ok` | literal `false` | Selects failure variant. | Other/mixed variant invalid. |
| failure `.error` | `Error` | Present only on failure. | Missing error or any result field invalid. |

Success is `{ok:true,result:GraphResult}` and failure is `{ok:false,error:Error}`. They are exclusive. Corrupt snapshots yield no best-effort result.

### Warning completeness (warnings-v1)

A `GraphResult` carries **exactly** one warning for each key `(code, provenanceId)` whose condition below holds, and no other warnings. A warning's `message` is free nonempty text, not part of its key or trigger. Evaluate only the returned request, coverage, provenance, and bindings on returned edges; evidence outside the result cannot add a warning.

| Code | Required if and only if | `provenanceId` |
|---|---|---|
| `syntaxOnly` | `request.semanticProducerId` is null. | null |
| `coverageIncomplete` | At least one returned *selected* coverage row is not complete. This is exactly the selected-coverage-incomplete operand of `partial`, not `partial` as a whole: a selected row has `state=failed` or `state=partial`. | null |
| `staleEvidence` | A returned `Provenance` has `freshness=stale`. | That provenance's `id`; one warning for each stale provenance. |
| `staleEvidence` | At least one returned `Provenance` has `freshness=possiblyStale`. | null; one aggregate warning, however many provenances are possibly stale. |
| `staleTarget` | A binding on a returned edge has `staleTarget=true`. | That binding's `provenanceId`; one warning per distinct provenance. |
| `bindingAmbiguous` | A binding on a returned edge has `resolution=ambiguous`. | That binding's `provenanceId`; one warning per distinct provenance. |

The aggregate `possiblyStale` key avoids repeating one warning for every affected document during ordinary manifest changes; each returned provenance still exposes its own freshness.

The coverage row rule requires all relevant rows to exist as specified above. A missing required row makes the result invalid; it is not a warning substitute. An unselected `omitted` or `unsupported` row alone does not trigger `coverageIncomplete`; an unsupported *requested role* represented by a selected `partial` row does. A frontier, boundary edge, or `partial=true` alone does not trigger that code. Conversely, independent coverage or freshness conditions still trigger their own warnings even when a boundary or frontier exists.

Deduplicate by `(code, provenanceId)` even when multiple edges share one binding provenance; two messages for the same key are invalid. Sort warnings by `(code, provenanceId, message)` using the declared **code enum order** and null-before-text tuple ordering in the conventions above, not lexical code order. A missing required warning, an extra warning without its condition, or an invalid/duplicate key invalidates the result. Unresolved and external bindings, dispatch boundaries, node limits, and truncation have no *dedicated* warning code; their `boundaryReason`, frontier, `partial`, and `truncated` fields retain their existing meanings. No new warning codes are introduced.

#### Worked warning checks

The following are hand-evaluated warning-key arrays; each assumes an otherwise valid result, a nonempty message for every listed warning, no unmentioned trigger, and the required coverage/provenance/edge fields. The arrays state `(code, provenanceId)` only because message text does not affect the required set.

1. **Syntax only.** The request has `semanticProducerId=null`, all returned selected coverage is complete, the root has no calls, and no returned provenance is non-fresh. `partial=false`; warning keys are `[(syntaxOnly,null)]`. No `coverageIncomplete` follows from syntax-only selection itself.
2. **Two ambiguous bindings, one provenance.** The request names a semantic producer; two returned edges have ambiguous bindings with `provenanceId=pA`, and all selected coverage is complete and returned provenance fresh. Both edges are boundaries, so `partial=true`, but warning keys are only `[(bindingAmbiguous,pA)]`—not two warnings and not `coverageIncomplete`.
3. **Three possibly-stale provenances.** Returned provenances `p1,p2,p3` each have `freshness=possiblyStale`; the request names a semantic producer, selected coverage is complete, and no returned edge binding is ambiguous or has a stale target. Warning keys are `[(staleEvidence,null)]`, not three per-provenance warnings. Each provenance still exposes its own freshness.
4. **Mixed freshness and a stale boundary.** Returned `p1` and `p3` are `possiblyStale`, `p2` is `stale`, and a returned edge binding captured at the requested revision has `provenanceId=p3`. `p3` is `possiblyStale` because the request compares its producer at a different version (same revision, same occurrences), so the edge is a `stale` boundary. `p2` is a historical declaration proof for a returned declaration in document `D2`. `D2`'s bytes changed since `p2`'s captured revision, and the producer's requested-revision tuple for `D2` is `omitted`, which is unselected and so adds no `coverageIncomplete`. The request names a semantic producer and selected coverage is complete. Warning keys, in code-enum and null-before-text order, are `[(staleEvidence,null),(staleEvidence,p2)]`; the stale boundary sets `partial=true` without adding `coverageIncomplete`. No `staleTarget` key arises in v1 (see `CallBinding.staleTarget`).
5. **Coverage versus other partial causes.** With a selected `partial` coverage row and an unselected `unsupported` row for a distinct relevant tuple, no other triggers, `partial=true` and warning keys are `[(coverageIncomplete,null)]`. If the selected row becomes `complete` while only the unselected `unsupported` row remains, no boundary or frontier exists, and all else stays fresh, then `partial=false` and `warnings=[]`. A depth frontier alone instead makes `partial=true` and `truncated=true` with `warnings=[]`.

### Pinned FIFO breadth-first algorithm

1. Pin the requested revision; resolve exact root; admit it at depth 0, count it against `maxNodes`, mark admitted on enqueue, and emit nodes in admission order.
2. FIFO-dequeue nodes. Sort that node's own calls by `(path UTF-8 bytes,start,end,call ID ASCII bytes)`. Nested callable bodies belong to their own owners. Emit edges in dequeue order then local source order, not global file or evaluation order.
3. At requested depth, emit no edges. If calls exist, append one depth frontier with null call/target, `nextOrdinal=0`, and exact measured call count; empty nodes add none.
4. Below depth, every emitted call—including boundary, self/cycle, and repeated target—consumes one call. An admissible already admitted target is `seen`, consumes no node, and is not enqueued again. An unseen admissible target is `new` at parent depth+1.
5. If unseen admission would exceed node cap, emit boundary `nodeLimit` with null `to`, then a per-call nodeLimit frontier containing call and refused target, null nextOrdinal, omittedCalls 0. Continue later calls, including seen targets.
6. Before the next call, if call budget is exhausted, stop globally. Add a callLimit frontier for current suffix with next call ordinal and remaining count. Drain queued nodes without edges: at depth they receive depth frontiers; below depth nonempty nodes receive callLimit from ordinal 0; empty nodes none. Exact numerical cap with no work remaining adds no frontier.
7. Semantic boundary precedence is missing binding/coverage=`missingEvidence`; non-fresh binding or stale target=`stale`; then ambiguous/unresolved/external resolution; then nonstatic=`dispatch`; only then node limit. A syntax-only measured call without a semantic binding is `missingEvidence`.
8. Retain frontier creation order and queued-node order. `truncated` iff frontier nonempty; `partial` iff truncated, incomplete selected coverage, or boundary edge.

No cursor, pagination, DFS, filter, execution ordering, or runtime-completeness semantics exist in v1.

### Hand-evaluated graph cases

Notation: `N(X,d)` is a node; `E(call,from,to,visit,reason)` is an edge; `F(reason,node,call,target,next,omitted)` is a frontier. Arrays below are the exact ordered logical arrays. Budgets are adequate unless stated.

1. **Source order versus evaluation.** Owner A has outer `c0:[10,20)` to F and nested `c1:[12,18)` to G. `nodes=[N(A,0),N(F,1),N(G,1)]`; `edges=[E(c0,A,F,new,none),E(c1,A,G,new,none)]`; `frontier=[]`; calls=2, nodes=3. The outer start sorts first despite evaluation intuition.
2. **Callback/method reference.** In owner A, the source expression `scheduler(C::run)` has one measured invocation, `a0`, whose fresh exact direct binding targets scheduler declaration S. `C::run` is a method-reference `Reference`, and callable body C owns no invocation caused by being passed. With depth 1, adequate budgets, and no measured calls owned by S, A's ordered call array is exactly `[a0]`. The hand evaluation is `nodes=[N(A,0),N(S,1)]`; `edges=[E(a0,A,S,new,none)]`; `frontier=[]`; calls=1, nodes=2, queued=0 after completion, `truncated=false`, `partial=false`. Thus the real scheduler invocation is the only emitted call from A. There is no measured or synthesized call `A→C`, no edge to C's body, and C is not admitted merely because the callback reference was passed.
3. **Cycle.** A has `a0→B`; B has `b0→A`; depth 2. `nodes=[N(A,0),N(B,1)]`; `edges=[E(a0,A,B,new,none),E(b0,B,A,seen,none)]`; `frontier=[]`; calls=2, nodes=2. A is never re-enqueued. Self-loop A→A: `nodes=[N(A,0)]`; `edges=[E(a0,A,A,seen,none)]`; `frontier=[]`; calls=1.
4. **Queued/visited diamond.** A local calls are `a0→B,a1→C`; B has `b0→D`; C has `c0→D`; depth 3. Admission/dequeue yields `nodes=[N(A,0),N(B,1),N(C,1),N(D,2)]`; `edges=[E(a0,A,B,new,none),E(a1,A,C,new,none),E(b0,B,D,new,none),E(c0,C,D,seen,none)]`; `frontier=[]`; calls=4. C sees D while D is queued, before D expansion.
5. **Depth.** Chain A→B→C, depth 1: `nodes=[N(A,0),N(B,1)]`; `edges=[E(a0,A,B,new,none)]`; `frontier=[F(depth,B,null,null,0,1)]`; calls=1; truncated/partial true. Depth 0: `nodes=[N(A,0)]`; `edges=[]`; `frontier=[F(depth,A,null,null,0,1)]`; calls=0.
6. **Node cap with later seen edge.** A calls in order `a0→B,a1→C,a2→A`, maxNodes=2. `nodes=[N(A,0),N(B,1)]`; `edges=[E(a0,A,B,new,none),E(a1,A,null,boundary,nodeLimit),E(a2,A,A,seen,none)]`; `frontier=[F(nodeLimit,A,a1,C,null,0)]`; calls=3. The refused C does not stop the later self-loop.
7. **Global call cap.** A calls `a0→B,a1→C`; B calls `b0→D`; maxCalls=1. After a0: `nodes=[N(A,0),N(B,1)]`; `edges=[E(a0,A,B,new,none)]`; `frontier=[F(callLimit,A,null,null,1,1),F(callLimit,B,null,null,0,1)]`; calls=1. The first frontier is created at A, then B is drained in queue order. With maxCalls=0 and root calls `[a0,a1]`: `nodes=[N(A,0)]`; `edges=[]`; `frontier=[F(callLimit,A,null,null,0,2)]`; calls=0. With maxCalls=1 and A's only call `a0→B`, B empty: arrays are nodes A,B, one new edge, `frontier=[]`; exact cap has no remaining work, so not truncated.
8. **Dispatch and freshness.** (a) A has c0 with a fresh resolved internal virtual binding and one possible target V: `E(c0,A,null,boundary,dispatch)`. `nodes=[N(A,0)]`; `edges` is that one; `frontier=[]`; calls=1. V is not admitted; possible dispatch remains only in the binding; partial is true. (b) An independent snapshot: A has c0 as in (a) plus c1 with an exact direct internal binding. The request compares the selected producer P at a different version than P's captured basis; within (b) the captured and requested comparisons share its one unchanged revision, source calls and occurrence IDs. Every P proof is then `possiblyStale`, and `stale` precedes `dispatch`: `E(c0,A,null,boundary,stale)`, `E(c1,A,null,boundary,stale)`. `nodes=[N(A,0)]`; edges are those two in source order; `frontier=[]`; calls=2. Neither V nor c1's target is admitted; partial is true.

## Artifact validity and failure disposition

| Condition | Disposition |
|---|---|
| Invalid shape/unknown field, dangling nonexternal reference, invalid range/encoding, impossible state/cardinality, digest mismatch, or conflicting duplicate key | Reject the affected producer artifact atomically. Publish none of its records. |
| Valid artifact intentionally lacks evidence | Publish `partial`, `unsupported`, `omitted`, or `failed` tuple coverage with required diagnostic; valid present evidence remains usable according to freshness. |
| Non-exact join | Preserve diagnostic evidence; install no binding. |
| Missing/corrupt pinned revision or invalid request | Return the typed failure only, never a partial result/current fallback. |
| Old evidence after failed refresh | Retain as labelled historical provenance; never bind it to a new revision or call it fresh. |

Valid partial evidence is therefore not a malformed partial artifact. Atomic rejection protects coherence; coverage states describe deliberate, well-formed incompleteness.

## Review checklist

The independent reviewer must mark every finite item before vectors are authored:

- [ ] Shared types plus every field of Producer, SourceSet, DocumentKey, Document, Revision, and Coverage: exact type, required/null rule, invariant, and failure.
- [ ] SemanticBasis and Provenance fields; manifest sorting and empty-prefix component hashes; tuple coverage independent from freshness.
- [ ] Declaration, SymbolKey, Symbol, both Target variants, DeclarationBinding, and TypeRelationship; syntax IDs never replaced by symbols.
- [ ] Range, MeasuredAnchor, Join, Call, ControlRegion, and Reference; coordinate conversion and exact/ambiguous/unmatched cases.
- [ ] Resolution's four cardinalities and every CallBinding field; contradictory exact evidence becomes ambiguous.
- [ ] Signature, Key, Header, Parameter, DurableAnchor, GroupContinuity, and AnchorResult; required nulls and all digest domains.
- [ ] GraphRequest defaults/limits and every GraphNode, GraphEdge, Frontier, Warning, GraphResult, and Error field.
- [ ] Malformed artifact atomic rejection versus valid partial publication.
- [ ] Two producers/document; partial-fresh; complete-stale; changed dependency/config/producer with same caller; missing basis; v1 rejection of `staleTarget=true` and wrong-revision internal targets (publication-rejoined targets use r2 IDs); failed refresh.
- [ ] All four lookup rules, measured spelling versus lookup normalization, seven roles, only Java alias N/A, and the per-language 40-reference-record counting boundary.
- [ ] Canonical bytes byte-for-byte; NUL is part of each domain; stable declaration input/exclusions; Java signatures; exact sibling grouping/ordinals; document-version-local `occ:v2` occurrences with revision-scoped semantic validity ([Decision 0003](decisions/0003-per-document-occurrence-ids.md)).
- [ ] Header projection limitations; group hashes/counts; continuity is independent; all six anchor scenarios and unknown same-count continuity.
- [ ] FIFO BFS admission/dequeue/local ordering and each explicit graph array: nested/evaluation, callback, cycle, self-loop, queued diamond, depth 0/1, node cap plus later seen, call cap/zero/exact, dispatch, producer-version stale boundary.
- [ ] Counters, frontier combinations/arithmetic, creation order, exact-cap behavior, boundary precedence, partial/truncated definitions.
- [ ] Wording never equates syntax, semantic binding, reference, call, or possible dispatch and never promises runtime completeness.
- [ ] Scope remains prospective: no production ID/API/storage/traversal migration and no executable checker claim in this slice.
