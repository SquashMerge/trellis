# Publication-rejoin v1 documentation vectors

**Prospective docs-only checks, not an executable decoder/corpus.** [Contract](contract-v1.md#publication-time-rejoin-of-captured-scip-evidence-11a) and [decision](decisions/0002-publication-rejoin.md) control these examples. Raw producer capture is separate from an independently measured native anchor and language-specific semantic proof. These exact digest rows were calculated independently with SHA-256 of the shown #22 one-line UTF-8 canonical bytes and checked against `tools/semantic-contract/json.mjs::canonicalBytes`; the binary examples were decoded with the local pinned `scip = 0.10.0` Rust crate, `protobuf = 3`.

## Complete two-document captured input

The admitted source set is `app` (root `root-app`, language `javascript`, no dependencies), captured revision `r1`, selected semantic producer `scip-test` version `1.0.0`, decoder `scip-0.10.0/CapturedScipFactV1/1`, declared producer position encoding `utf8`. Native r2 is an independently measured revision (`r2 != r1`) with A and B source bytes unchanged. Under [Decision 0003](decisions/0003-per-document-occurrence-ids.md), whether A's occurrence IDs stay equal depends on *what* changed: a native producer version change, or a change to a component the native producer reads (e.g. its config capture), re-identifies them, while a change only to a capture the native producer does not read keeps them equal. The `occ:v2` rows below fix that case explicitly. For a changed-B subcase, use `B2="function g(){return 1;}\n"` (hex `66756e6374696f6e206728297b72657475726e20313b7d0a`, byte length 24, SHA-256 `47ae88cf464c589aaa9adf0441a6d7c63245b417a3e78a975aefab2f4af1c53f`); A bytes stay identical. The r2 source manifest and revision ID must change; the B.g stable descriptor/ID below remains the same when uniquely remeasured. The complete two-document authenticated source manifest is the following #22 canonical array, in language/path order; no other documents are in this admitted fixture:

```json
[{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","document":{"language":"javascript","path":"src/A.js","sourceSetId":"app"}},{"contentHash":"ae7703c040471d7d8cf8032c1d4acf32d03fb3ea089cc341f72e91cfcb3f24b5","document":{"language":"javascript","path":"src/B.js","sourceSetId":"app"}}]
```

`sourceManifestHash` (empty-prefix SHA-256 of the exact array above) = `5578e3d23b2cba76685271de456413c9b207379613d72f1b55b25d84a487cf95`. Source bytes and hashes, with **no** newline added outside the shown hex:

| Document | UTF-8 source | Source hex | SHA-256 | byte length |
|---|---|---|---|---:|
| A | `function f(){g();}\n` | `66756e6374696f6e206628297b6728293b7d0a` | `8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2` | 19 |
| B | `function g(){}\n` | `66756e6374696f6e206728297b7d0a` | `ae7703c040471d7d8cf8032c1d4acf32d03fb3ea089cc341f72e91cfcb3f24b5` | 15 |

Additional authenticated captured manifest components (the component hashes are empty-domain SHA-256 of the exact bytes): producer executable bytes `736369702d746573742d62696e` => `f615261a2fb15f6d51cac723cdbe8c3fe3fb495090f43b3cffc226bb3be278ed`; toolchain bytes `746f6f6c2d7631` => `dc0d766981c85d12f41dec89bf8cddb4a1629265e25265ec02189226b1deebdd`; config bytes `636f6e6669672d7631` => `e3155b20e134632816c8611c4e9ee5cbd0e00689f7c4c955ee9f896580d02fdb`; dependency bytes `646570732d7631` => `48ce14bab85b92d43dca7de13bff5155ddae7c9017930ecbf0f10c20cd028529`. The captured `SemanticBasis` binds these hashes, `producerId=scip-test`, `producerVersion=1.0.0`, `language=javascript`, `sourceSetId=app`, `revisionId=r1`, and the artifact and manifest hashes below; `lookupDependencies=[]` for this fixture. The authenticated source admission binds both `DocumentKey`s to `root-app` and its exact source bytes; the externally authenticated manifest, **not** protobuf metadata guessed by a decoder, attests these components. Native source revision `r1` itself does not include semantic artifacts. This fixture does not invent an on-disk importer manifest wire schema.

SCIP protobuf `Index` wire hex below has A with three occurrences (legacy definition and typed single-line/multiline uses), one `SymbolInformation` with one relationship (both `is_reference` and `is_implementation` true), and B with no facts. `Index.metadata` and `Document.text` are absent; index/SCIP source binding comes from the authenticated manifest above. It is **116 bytes** and was decoded with `scip 0.10.0` as two documents, A `position_encoding=1`, legacy `[0,9,10]`, typed single `(0,13,14)`, typed multi `(0,13,0,14)`, symbol kind `17` (Function), flags `(true,true,false,false)`, B `position_encoding=1`.

```text
12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a6176617363726970743001
```

`artifactHash = SHA-256(binary)` = `8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b`. The source manifest, binary, producer bytes/profile and pinned decoder must all authenticate **before** facts are selected or joined. An omitted relationship flag is its actual decoded protobuf default `false`, not an optional null. Present and decoded-default numeric values are represented equally in v1 canonical envelopes.

## Exact producer envelopes, canonical bytes and full hashes

For each row, the JSON line is the **complete** #22 `canonicalBytes(E)`; no newline belongs to the digest. Compute `D(E)=SHA-256(UTF8("trellis.semantic-fact.v1\0")||canonicalBytes(E))`. Compute captured proof ID with `SHA-256(UTF8("trellis.capture-proof.v1\0")||canonicalBytes(C))`, where `C` is the exact seven-key tuple below. These five rows are the complete deduplicated **document-only** envelope inventory of the authenticated 116-byte binary (three occurrences, one symbolInformation, one separate relationship). Index-level external symbols are authenticated binary context and never enter the envelope/proof inventory. These rows are not destination records and do not assert a call or relationship classification.
### call-occurrence
`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"singleLine","endCharacter":14,"line":0,"startCharacter":13},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}
```
`factDigest = 138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc`.
`canonicalBytes(C)`:

```json
{"artifactHash":"8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b","factDigest":"138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```
`capturedProofId = 8d91e854886b2b9f02aaa8dc4a240714a5ca4de8b144b3cf6dd430219100a0e0`.
### reference-occurrence
`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"multiLine","endCharacter":14,"endLine":0,"startCharacter":13,"startLine":0},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}
```
`factDigest = e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f`.
`canonicalBytes(C)`:

```json
{"artifactHash":"8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b","factDigest":"e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```
`capturedProofId = 74f8160501fb821c4554cc2671bb843eda75fd17294466d94213f547687807fc`.
### definition-occurrence
`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,9,10]},"symbol":"A#","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}
```
`factDigest = 40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6`.
`canonicalBytes(C)`:

```json
{"artifactHash":"8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b","factDigest":"40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```
`capturedProofId = 8e02c380dd47ad9a2f83cfeb0ffa138a7925817ee00207d42a9727849bb07dca`.
### symbol-information
`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"displayName":"f","enclosingSymbol":"","kind":17,"symbol":"A#"},"recordKind":"symbolInformation","relativePath":"src/A.js"}
```
`factDigest = 39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f`.
`canonicalBytes(C)`:

```json
{"artifactHash":"8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b","factDigest":"39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```
`capturedProofId = c1311eda3c429295f8f161484b0831f57d4eacb2854ad92204fbde79a4eb7c77`.
### relationship
`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"isDefinition":false,"isImplementation":true,"isReference":true,"isTypeDefinition":false,"sourceSymbol":"A#","targetSymbol":"B#"},"recordKind":"relationship","relativePath":"src/A.js"}
```
`factDigest = e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0`.
`canonicalBytes(C)`:

```json
{"artifactHash":"8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b","factDigest":"e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```
`capturedProofId = c07c6266d07c757383acd60e0dedc46a05ff560bb3b1ccbfbaa1c5ce4c3b669c`.
## Index-level context and range-branch controls

An otherwise identical Index with `external_symbols=[{symbol:"external#"}]` appends exactly `1a0b0a0965787465726e616c23` to the 116-byte binary. The complete 129-byte Index wire and SHA-256 are:

```text
12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a61766173637269707430011a0b0a0965787465726e616c23
```

`19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f`. **Positive:** `scip 0.10.0` parses one index-level `external#` symbol, which is recursively checked (including nested relationships/unknown fields), authenticated by this binary hash, and omitted from envelope/proof inventory. The five document envelopes above have **unchanged fact digests**, but their proof IDs change because the artifact hash in every document claim tuple is now the 129-byte hash. **Negative:** it cannot create a local A declaration/TypeRelationship, A coverage or r2 local provenance; neither attaching it to the first document nor inferring a document language is allowed. If a nested index-level message has an unknown field/enum, reject the entire captured artifact rather than silently ignoring it.

For completeness, the following **changed-artifact proof tuples** bind the 129-byte binary. Each full `C` is shown: `Index.external_symbols` adds **zero** document envelopes/digests, yet changes the authenticated artifact hash and therefore the five document proof IDs.

| Existing document envelope | #22 canonical `C` with 129-byte artifact | new `capturedProofId` |
|---|---|---|
| `call-occurrence` | `{"artifactHash":"19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f","factDigest":"138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}` | `5f06a5a26e82a671ac92c9f60ecd804aa0a1161e9933ae6b3b96b3eb77f5e9b6` |
| `reference-occurrence` | `{"artifactHash":"19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f","factDigest":"e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}` | `3a5a01c21df6e88eca70514edb3d9433e0b03ae9a84eb0cad409aa4641f314d1` |
| `definition-occurrence` | `{"artifactHash":"19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f","factDigest":"40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}` | `82b53fe23ca439ff9517ce43979039dce1b2d4cbd7ee59c316801c3c3cc1700a` |
| `symbol-information` | `{"artifactHash":"19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f","factDigest":"39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}` | `aadefd7776bc74f80c4b600b3e0421e9bc0c4122cec7b6125a085721855299c1` |
| `relationship` | `{"artifactHash":"19d1e39011338e4cae9f73289c851db75beebd69f221e5295c57540eb9afd23f","factDigest":"e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}` | `f5f084bbb2ac6624e3e5f2d7d8b5550a40e355523dee30d9906edf2525e21e97` |

The corrected **44-byte** legacy-only Index wire is:

```text
122a0a087372632f412e6a73220a6a61766173637269707412100a03000a0e12076c6f63616c203018013001
```

Its empty-prefix artifact hash is `1f62fd7188854b19ebad46d5d1048193263fcd1b37cfef7fb7d1d6c35d417409`. `scip 0.10.0` decodes one JavaScript `src/A.js` Document, encoding 1 and packed legacy `[0,10,14]` (`local 0`, role 1). For this standalone demonstration the authenticated source is `0123456789call\n` (hex `3031323334353637383963616c6c0a`; hash `09e4f3066833d4d73ff18e98cbb31d0e5674d3bf7619f789b69e916eab49dc9a`). Its document-only canonical envelope and full digest are:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,10,14]},"symbol":"local 0","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}
```

`D(E) = 6d600757051c743f49830194c6d0cf5d78b27c666fbf60e3ce95100384d45670`. This restores the draft digest for this exact envelope shape; it does not reinterpret legacy #26 `SemanticCapture` or assume `local 0` is a call.

The following two additional admitted branch inputs are carried by a separate complete one-document Index. Its admitted document `src/A.js` has exactly the same A source bytes and hash from above; its entire manifest is `[{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","document":{"language":"javascript","path":"src/A.js","sourceSetId":"app"}}]` with empty-prefix manifest SHA-256 `52b4124248408e1a37e7ff17b786ff57a998b54b3ebfe252e478330d203506ee`. Its exact 70-byte `scip 0.10.0`-decoded binary (legacy four with typed single-line enclosing; typed single-line primary with typed multi-line enclosing) is:

```text
12440a087372632f412e6a73220a6a61766173637269707412120a04000d000e1202422352060800100c1810121642060800100d180e120242235a080800100c180020103001
```

`artifactHash = 2b3c07f5e087eecb1b0ccb18627b0b66c60eb2bbde5a083e918323d4034e6d2b`. The other authenticated profile/component bytes are unchanged, but this is **not** the 116-byte artifact and proof tuples must bind this artifact hash. `enclosingRange` is either exactly one validated branch or null. Each typed zero is a present canonical JSON numeric field even where protobuf omitted the zero on the wire.

- `legacy-four` canonical: `{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":{"encoding":"singleLine","endCharacter":16,"line":0,"startCharacter":12},"range":{"encoding":"legacy","values":[0,13,0,14]},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}`; `D(E)=93671bccac30c539d7d133849995f8798f3e4ba7a56804e4577ab108870e30c4`.

- `enclosing-multi` canonical: `{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":{"encoding":"multiLine","endCharacter":16,"endLine":0,"startCharacter":12,"startLine":0},"range":{"encoding":"singleLine","endCharacter":14,"line":0,"startCharacter":13},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}`; `D(E)=c7cbbbd7c3e447e4f3d1e5438b361d94b755627dc66f4c9601a0f6449e6b18bd`.

Reject primary empty/missing range, legacy length 2 or 5, negative/out-of-int32 coordinate, mixed legacy and typed (including identical-looking values), two competing typed oneof tags even if a protobuf decoder silently overwrites one, and mixed legacy/typed enclosing range. Detect competing oneof tags from authenticated wire before trusting the parsed last value. Reject unknown `Document.position_encoding`; raw 0 is admitted **only** with a valid authenticated producer-version manifest encoding fallback for conversion. Neither a guessed UTF-16 default nor conversion without valid artifact/manifest encoding is allowed. UTF-16 coordinate splitting a surrogate pair, UTF-8 byte coordinate splitting a scalar, UTF-32 coordinate past line end, reversed span, scalar boundary outside source or a CRLF coordinate that includes the `\r` as a phantom part of the next line fails before join. Example source `"😀\r\nx"`: UTF-16 line 0 offset 1 splits the surrogate; UTF-8 line 0 offset 1 splits the four-byte scalar; line 1 starts only after both CRLF bytes and `x` is at line 1 column 0. A typed range with `{line:0,startCharacter:0,endCharacter:0}` is zero-length and cannot support an ordinary nonempty reference anchor. A valid converted primary span with no compatible measured destination candidate is a join failure, not an invalid range.

Duplicate `Document.relative_path=src/A.js` in one Index rejects the entire captured artifact before extraction even when one entry declares another language (it must not overwrite/merge). Change only presentation-only documentation or diagnostic: envelope digest stays the same, binary artifact hash and proof context change; an old artifact manifest hash now fails. Unknown protobuf fields anywhere, including omitted nested diagnostics/signatures/metadata; unknown enum numeric values/role bits (e.g. role 128); nonmatching `Document.text`; fake manifest path/source/producer bytes; or a new unknown semantic extension under an omitted message rejects or requires an explicit envelope/domain version change. Known nonempty `Signature.occurrences` remain presentation-only, verified under the artifact hash and excluded from source joins. No unknown field can vanish in a protobuf round-trip before validation. Changing raw symbol, role, range or encoding changes the envelope digest; doing so without updating binary/manifest/inventory rejects the artifact. Index metadata/display fields remain authenticated via original binary hash, never silently promoted into local semantic facts.

## Raw-zero encoding and nonempty signature-occurrence controls

The next independent one-document captured input uses `src/A.js` and **the same authenticated A source bytes** `66756e6374696f6e206628297b6728293b7d0a` (SHA-256 `8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2`). Its complete source manifest is `[{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","document":{"language":"javascript","path":"src/A.js","sourceSetId":"app"}}]`, SHA-256 `52b4124248408e1a37e7ff17b786ff57a998b54b3ebfe252e478330d203506ee`. This is a complete logical capture context: the signed/pinned producer profile, original artifact bytes/hash, decoder version and manifest bind the fallback encoding. The exact serialization of the future importer manifest is a separate 11B choice, not a retroactive #26 format change. For this positive example, the authenticated manifest/profile attests this example tuple (the **declared** `positionEncoding:1` means UTF-8 **for conversion**, not a mutation of raw SCIP `0`):

```json
{"artifactHash":"9bd3ef0f2a6b9aeef556af27a8a424d2cf21a2f18adf8ab567a82b1677a24a59","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","language":"javascript","positionEncoding":1,"producerHash":"f615261a2fb15f6d51cac723cdbe8c3fe3fb495090f43b3cffc226bb3be278ed","producerId":"scip-test","producerVersion":"1.0.0","rootId":"root-app","sourceManifestHash":"52b4124248408e1a37e7ff17b786ff57a998b54b3ebfe252e478330d203506ee","sourceRevision":"r1","sourceSetId":"app"}
```

The other captured toolchain/config/dependency component bytes and hashes are the ones declared in the complete two-document fixture above; document A's source set/root and hash are the same. The Index wire below is **78 bytes**. Its `Document.position_encoding` field is **omitted on wire**, so `scip 0.10.0` decodes raw value `0`. It includes a source-document definition occurrence `[0,9,10]` and a `SymbolInformation.signature_documentation` with `language="javascript"`, `text="f()"` and **one** separate signature-text occurrence `[0,0,1]` (`A#`). Native `scip 0.10.0` decoded `position_encoding=0`, `Document.occurrences.len()=1`, `Signature.occurrences.len()=1`; that signature occurrence never becomes a second source-document occurrence or a local join anchor.

```text
124c0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a270a02412328113201663a1c220a6a6176617363726970742a0366282912090a0300000112024123
```

`artifactHash = 9bd3ef0f2a6b9aeef556af27a8a424d2cf21a2f18adf8ab567a82b1677a24a59`. The exactly two document-only envelopes and complete #22 canonical context/digest/proof vectors are:

### raw-zero occurrence

`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":0,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,9,10]},"symbol":"A#","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}
```

`D(E) = 48eecbae6ce056236cc3decb2fb3c25c46bcd2fe54cc9badb5fb9a12ae318dc7`. Its raw `positionEncoding:0` remains `0` in digest bytes; only conversion consults the authenticated manifest 1.

`canonicalBytes(C)`:

```json
{"artifactHash":"9bd3ef0f2a6b9aeef556af27a8a424d2cf21a2f18adf8ab567a82b1677a24a59","factDigest":"48eecbae6ce056236cc3decb2fb3c25c46bcd2fe54cc9badb5fb9a12ae318dc7","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```

`capturedProofId = f5399701a43a9ebf56cd5766360985d57f066a2c8093d8f07361fac3b11f042f`.

### raw-zero symbolInformation

`canonicalBytes(E)`:

```json
{"formatVersion":1,"positionEncoding":0,"raw":{"displayName":"f","enclosingSymbol":"","kind":17,"symbol":"A#"},"recordKind":"symbolInformation","relativePath":"src/A.js"}
```

`D(E) = 68953b47a29e361cf99726fe506d7449dfc9a4c2c1a250aa0a6f52d0a96cc561`. Its raw `positionEncoding:0` remains `0` in digest bytes; only conversion consults the authenticated manifest 1.

`canonicalBytes(C)`:

```json
{"artifactHash":"9bd3ef0f2a6b9aeef556af27a8a424d2cf21a2f18adf8ab567a82b1677a24a59","factDigest":"68953b47a29e361cf99726fe506d7449dfc9a4c2c1a250aa0a6f52d0a96cc561","language":"javascript","producerId":"scip-test","relativePath":"src/A.js","sourceRevision":"r1","sourceSetId":"app"}
```

`capturedProofId = cf5308c59658fb5523ffa0c4dd20f7b6bd372bff920073949af430a6f8fc3825`.

**Signature omission positive:** removing only `signature_documentation` from this Index gives the complete 48-byte wire `122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166`, hash `a4d1f5ce282cd4c3eecc58992c2badafaa4a590a943564345700f07203ac341b`. The **same two envelope canonical bytes and fact digests** remain. Their proof IDs change, solely because the authenticated binary hash changes: occurrence `1ad7595f3e9fd3d984cf62c564cd1e72f48e912389a83d5a7b67abefb3fb3328`; symbolInformation `3eef2daff2d27a4dcdbc9d7e75ed9575ba60c47538ff362f80471f6100b411ac`. An original artifact-hash manifest that still names the 78-byte version **rejects** this modified 48-byte binary until newly authenticated; matching fact digests alone never authenticate it. A presentation-only nonempty signature occurrence is allowed, but malformed unknown nested fields/enums still reject recursively.

**Negative:** the same 78-byte raw-zero binary with no authenticated producer-version position encoding in **either** manifest or producer descriptor, or with both declarations `0`/unknown, has no declared conversion convention and atomically fails admission; do not guess UTF-16. A declared valid encoding inconsistent with a valid artifact value 1/2/3 also fails. A manifest that lies about A source bytes, producer version, root, artifact hash or decoder fails before conversion. A valid raw `0` may produce a different `D(E)` from otherwise identical raw `1` because raw encoding is itself a digest field, even when both convert via the same authenticated UTF-8 convention. An explicitly encoded protobuf zero is equal to omitted zero for this envelope because protobuf field presence does not enter the canonical shape.

## Measured native anchors and five destination-family gates

Native extraction of the authenticated **source strings**, independent of the SCIP role bits, can measure A top-level function `f` name `[9,10)`, invocation `g()` `[13,16)`, callee/reference `g` `[13,14)` with owning callable `f`; B top-level function `g` name `[9,10)`. All are zero-based half-open UTF-8 byte offsets and scalar boundaries. The native measurements used here are controlled fixtures; a producer's raw symbol does not prove that a source adapter actually emitted them. Stable descriptor inputs (`Key={kind:"function",name,signature:null,ordinal:0}`, empty ancestors, sourceSet `app`, language `javascript`) yield:

| Measured declaration | #22 canonical stable input | Full SHA-256 / emitted syntax ID |
|---|---|---|
| `A.f` | `{"ancestors":[],"declaration":{"kind":"function","name":"f","ordinal":0,"signature":null},"language":"javascript","path":"src/A.js","sourceSet":"app"}` | `7fd250597c82d08fcb73cabd62e89893b1f5626ea7cb8006016cd9fe6bf4e306` / `sid:v1:7fd250597c82d08fcb73cabd62e89893` |
| `B.g` | `{"ancestors":[],"declaration":{"kind":"function","name":"g","ordinal":0,"signature":null},"language":"javascript","path":"src/B.js","sourceSet":"app"}` | `feaeb92ee1e041830ef648299f966271acd8e121268b2c19f4bb777d6d98b303` / `sid:v1:feaeb92ee1e041830ef648299f966271` |

<a id="occurrence-identity-v2-decision-0003"></a>
For A's one measured invocation and one measured use-reference (ordinal 0 within each occurrence kind), the exact #22 occurrence inputs and outputs follow. They use the `occ:v2` identity of [Decision 0003](decisions/0003-per-document-occurrence-ids.md) (proposed), which **replaces** the withdrawn revision-bound v1 rows.

The fixture native producer is `native-test` version `native-test-1`. It **declares and reads exactly one component, `config`**, in every context. Its extraction context uses this fixture's authenticated config capture (`config-v1`, bytes `636f6e6669672d7631`, hash `e3155b20…` above). For the equal-ID rows, r2 differs from r1 **only** in the dependency capture, which the native producer does not read. A's bytes (`contentHash` 8026dced…), producer and extraction context are therefore unchanged, and **A's r1 and r2 occurrence IDs are identical** (r1 ID = r2 ID). The two controls are separate:
- a native producer version change at the same context;
- a change to the authenticated config capture, at the same declared inventory and producer version, using a separately identified **hypothetical** second capture `config-v2` (bytes `636f6e6669672d7632`).

Extraction contexts (domain `trellis.extraction-context.v1\0`):

| Context | canonical input | SHA-256 |
|---|---|---|
| A, authenticated `config-v1` capture | `{"components":[{"hash":"e3155b20e134632816c8611c4e9ee5cbd0e00689f7c4c955ee9f896580d02fdb","name":"config"}],"language":"javascript"}` | `b49d85d64bb03f7cf62bd68c08f2a1aa091107769c325315528593d5724b12bc` |
| A, hypothetical `config-v2` capture (control) | `{"components":[{"hash":"3e8214adf35212b25f8d669f6bb1416d39e07bacc3d82acfc287218cabbe0712","name":"config"}],"language":"javascript"}` | `1f2a121e20976ce130efa9cafff90f03229eab048982670f3a67fcf6b6b27ded` |

Occurrences (domain `trellis.occurrence.v2\0`; all but the last row use the `config-v1` context):

| Case / kind | canonical input | SHA-256 / `OccurrenceId` |
|---|---|---|
| r1 ID = r2 ID, `call` | `{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","extractionContext":"b49d85d64bb03f7cf62bd68c08f2a1aa091107769c325315528593d5724b12bc","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893"}` | `38ec6920f23b3e1f7bb35550d019bbb622a70d87072c1bf6aba9c19974237cae` / `occ:v2:38ec6920f23b3e1f7bb35550d019bbb6` |
| r1 ID = r2 ID, `reference` | `{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","extractionContext":"b49d85d64bb03f7cf62bd68c08f2a1aa091107769c325315528593d5724b12bc","kind":"reference","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893"}` | `3445e26a698a121224048f0b37496cea58bc2d4b3a19a64d56851d57891041dc` / `occ:v2:3445e26a698a121224048f0b37496cea` |
| native producer version changed (control), `call` | `{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","extractionContext":"b49d85d64bb03f7cf62bd68c08f2a1aa091107769c325315528593d5724b12bc","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-2","ordinal":0,"ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893"}` | `76c6e398193fe21ee1d9915dbbdee28db8d78c0e88d5c36d601c3b3787a4dd1f` / `occ:v2:76c6e398193fe21ee1d9915dbbdee28d` |
| config capture changed (control), `call` | `{"contentHash":"8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2","extractionContext":"1f2a121e20976ce130efa9cafff90f03229eab048982670f3a67fcf6b6b27ded","kind":"call","nativeProducerId":"native-test","nativeProducerVersion":"native-test-1","ordinal":0,"ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893"}` | `7850439bee41f2d40363662d283ffec3199ee742e3bbcf8c4d25e85968da6394` / `occ:v2:7850439bee41f2d40363662d283ffec3` |

These rows were computed from #22 canonical bytes with `tools/semantic-contract/json.mjs::canonicalBytes`, and independently re-canonicalized and hashed with Python `hashlib`; `SHA-256("config-v1")` matches the authenticated capture above. The same method reproduces the withdrawn v1 `r1/call` digest (`959c5606…`). "r2 call ID above" in the rows below means this v2 ID, equal to r1's in the equal-ID setup. Rejoin still requires a separately verified r2 native candidate and mints **new r2 provenance and associations**; ID equality never makes an r1 binding valid at r2.

Only a separately established semantic binding can select B.g as an internal callee/reference. A source candidate is exactly one **distinct** compatible r2 native ID at its converted span/kind/document/source set; two raw facts matching that one native candidate remain two proofs, not two measured anchors. A selected direct dispatch requires additional independent producer/language evidence; raw SCIP symbol roles 0 or 1 cannot establish direct dispatch, and even an independently proven direct binding with r1 basis cannot expand because it is `possiblyStale`. The following positives are **conditional** on the explicitly stated independent semantic evidence, not inferred from the wire:

| Family | Raw witness + independent measurement/semantic assertion | r2 outcome | Negative control |
|---|---|---|---|
| `CallBinding` | `call-occurrence`; measured A `[13,16)` invocation and `[13,14)` callee; independently established resolved B.g target and supported dispatch | r2 call ID above, owner A.f, r2 B.g target, minted derived proof (`recordKind:occurrence`); `possiblyStale`, `staleTarget=false`, no expansion | Two distinct measured A calls compatible at callee span → A selected `failed`; B missing while independent A reference succeeds → omit call alone, A `partial`. |
| `Reference` | `reference-occurrence`; independently measured A use at `[13,14)` plus proven B.g resolution | r2 reference ID above and r2 B.g target, proof from raw occurrence | No compatible measured A reference → A `failed`; B target missing/nonunique alone → omit dependent reference and A `partial`. |
| `DeclarationBinding` | `definition-occurrence` role 1, **independently** measured A.f declaration name `[9,10)` and semantic symbol A# | exact r2 A.f syntax ID and binding; raw role alone does not supply ID | Two distinct compatible measured A declarations at `[9,10)` → A `failed`, not first-wins. |
| `Symbol` internal declaration target | `symbol-information` together with independently joined `definition-occurrence` and A.f measurement; source `A#` alone is insufficient | r2 A.f target; both raw proofs remain distinct, each minted derived link uses its own raw kind/digest | Missing A.f source → A `failed`; a dependent missing B target omits only that dependent claim and makes A `partial`. |
| `TypeRelationship` | `relationship` flags `(true,true,false,false)` preserved; **only** in a separate language/source fixture that independently measures compatible subtype/base declarations and establishes actual `extends|implements|overrides` classification/direction | r2 same-ID source/target and raw relationship proof (`recordKind:relationship`), `possiblyStale`; no type relation follows from this function-only binary | This JavaScript A.f→B.g binary has no verified type relationship, so it must **not** create one. An independently proven type fixture with missing/nonunique B target omits only its dependent relationship, makes A `partial`; ambiguous A type source fails A. |

A single raw `occurrence` can support both a call and reference **only when** independent native measurements and family-specific semantic facts support both: reuse its `factDigest`/captured proof ID, never add a destination-family discriminator or duplicate claim. If two distinct envelopes share one compatible native source candidate, join remains exact and each provenance survives; conflicting proven targets produce `resolution:ambiguous` with sorted candidates, not first-wins. A raw relationship containing both reference and implementation flags is valid, but those bits cannot select a Trellis relation kind. The TypeRelationship positive above is a conditional gate vector, not a claimed type proof from the shown function-only source. No fabricated call or class declaration enters the authenticated fixture.

## Separate source-derived type-relationship positive

This **independent** admitted fixture has the same source set/profile/component bytes as above, revision r1, but its complete manifest consists solely of `src/types/A.js` and `src/types/B.js` (no paths from the function fixture). The exact source bytes are `636c617373204120657874656e64732042207b7d0a` (`class A extends B {}\n`, SHA-256 `7bc0517d8dbd4f7c8578e866904f32fb64ed6fb4f7dd23ff94cfc7170d1185ce`) and `636c6173732042207b7d0a` (`class B {}\n`, SHA-256 `18ba722e8c0cbb2aa3d7d5a8c79f0e65b50318e1391aae91b462444b6cb39f60`). Its complete #22 manifest and hash are:

```json
[{"contentHash":"7bc0517d8dbd4f7c8578e866904f32fb64ed6fb4f7dd23ff94cfc7170d1185ce","document":{"language":"javascript","path":"src/types/A.js","sourceSetId":"app"}},{"contentHash":"18ba722e8c0cbb2aa3d7d5a8c79f0e65b50318e1391aae91b462444b6cb39f60","document":{"language":"javascript","path":"src/types/B.js","sourceSetId":"app"}}]
```

`sourceManifestHash = 1474916bdab5c2922bff462165013f38a8f17c58afedd3f1bc93762faa4abd05`. The full **122-byte** protobuf Index below has A definition `[0,6,7]`, A use of B `[0,16,17]`, A symbol information with a separate raw relationship to B preserving `(isReference,isImplementation,isTypeDefinition,isDefinition)=(true,true,false,false)`, and B definition `[0,6,7]`; both documents explicitly declare position encoding 1. No other facts are in this fixture. The exact wire and artifact hash are:

```text
124b0a0e7372632f74797065732f412e6a73220a6a617661736372697074120b0a0300060712024123180112090a03001011120242231a130a02412322080a0242231001180128073201413001122b0a0e7372632f74797065732f422e6a73220a6a617661736372697074120b0a030006071202422318013001
```

`9943b2b703c03c7112edad95a26d99656d106054d2d1ab498bd6edd708473516`. The relation envelope's complete #22 canonical bytes, digest and proof tuple are:

```json
{"formatVersion":1,"positionEncoding":1,"raw":{"isDefinition":false,"isImplementation":true,"isReference":true,"isTypeDefinition":false,"sourceSymbol":"A#","targetSymbol":"B#"},"recordKind":"relationship","relativePath":"src/types/A.js"}
```

`factDigest = b8640d2c78bff6812eb6f18773006196790b920c167229d3c3ad5a68819d1795`.

```json
{"artifactHash":"9943b2b703c03c7112edad95a26d99656d106054d2d1ab498bd6edd708473516","factDigest":"b8640d2c78bff6812eb6f18773006196790b920c167229d3c3ad5a68819d1795","language":"javascript","producerId":"scip-test","relativePath":"src/types/A.js","sourceRevision":"r1","sourceSetId":"app"}
```

`capturedProofId = 46de15efef7c37ff9ef3e743fe3718f3c9249afc773b5c2350d5450cfd65e94f`. Native parsing **independently** measures A class name byte `[6,7)`, B class name byte `[6,7)`, and A `extends B` type-syntax reference `[16,17)`; the raw flags alone do not prove an `extends` destination kind. Native descriptor inputs and full SHA-256 / stable ID pairs are:

- `{"ancestors":[],"declaration":{"kind":"type","name":"A","ordinal":0,"signature":null},"language":"javascript","path":"src/types/A.js","sourceSet":"app"}` → `f8ab4674617c0a96f1949c78a90b2628795cc8d3f05856d7e60996b04ea08102` / `sid:v1:f8ab4674617c0a96f1949c78a90b2628`.
- `{"ancestors":[],"declaration":{"kind":"type","name":"B","ordinal":0,"signature":null},"language":"javascript","path":"src/types/B.js","sourceSet":"app"}` → `2895c04f3c8a538d3d626ab46f48f1fd50c50065dbb38fdbd8ddcd764374b431` / `sid:v1:2895c04f3c8a538d3d626ab46f48f1fd`.

When the language-specific semantic assertion independently establishes that A extends B (consistent with native `extends` syntax and verified symbol-resolution) and uniquely measures both same-ID r2 declarations, mint r2 `TypeRelationship{kind:extends,source:A,target:B}` with r2 internal targets, **new** A provenance derived from captured relationship proof above, captured r1 basis, `possiblyStale`, and no expansion. If B changes but exactly one compatible same-ID r2 declaration survives, A can retarget it. If B fails same-ID measured verification, omit only dependent relation, keep independent A definition/binding and A `partial` with diagnostic; if A source class anchor is missing/ambiguous, fail all A. The flags `(true,true,false,false)` are valid but do **not** mechanically imply `extends`; absent independent language-specific relationship proof, emit **no** TypeRelationship even though the captured envelope and digest remain valid.

## Publication, failure, history and warnings

For each minted r2 semantic provenance, use a **new** unique destination ID, `revisionId=r2`, A destination key/hash, original `SemanticBasis(revisionId=r1)` unchanged and `derivedFrom:{provenanceId:<listed capturedProofId>,recordKind:<listed raw kind>,factDigest:<listed D(E)>}`. Captured proofs retain `derivedFrom:null`. Reject a claim using another row's digest, unknown ID, forged tuple, missing envelope or two distinct proof bindings; identical raw envelopes within one authenticated tuple collapse before proof lookup and do not cause ambiguity. Distinct raw bytes/context do not collapse.

- Unchanged A, changed B bytes and **one uniquely measured compatible B.g declaration with the same verified stable ID**: rejoin A's dependent target into B at r2; B itself is not rejoined. A minted proof is `possiblyStale`, `staleTarget=false`, no traversal expansion. If B.g is absent or not uniquely measured, omit only dependent A call/reference/relationship claims; keep independent A declaration fact and `partial` A coverage with `observedRoles=[definition]` if definition was the only delivered role; diagnostic names omitted roles. Do not fabricate external B. A malformed source anchor instead selects `failed` A, zero A facts.
- A malformed binary, source admission, manifest hash, inventory, component hash, encoding, or conversion rejects the **whole producer artifact**. If another path publishes r2, each requested attributable P document tuple is selected `failed`, diagnosed, with zero P facts; another producer can succeed independently. Cancellation exposes no partial revision. A valid destination failure may coexist with successful tuples in one native publication transaction. A later explicit import races via CAS and on loss publishes no partial overlay.
- Changed A bytes: no A occurrence rejoin. If A r2 tuple is `failed|omitted`, #47 returns only latest earlier eligible `complete|partial` declaration-keyed provenance for **returned** r2 same-ID declarations in that document, plus that earlier tuple's coverage row even if no fact names a returned declaration. Older-than-latest, cross-document, unrelated declaration and occurrence-keyed evidence remain absent. Changed bytes → `stale` historical provenance; unchanged bytes but different revision → `possiblyStale`. If r1 latest is `partial`, do not reach back to an earlier complete tuple. No old call binding returns.
- Warning keys for selected A `partial` or `failed` with returned minted possibly-stale proof: `[(coverageIncomplete,null),(staleEvidence,null)]`; if stale historical proof `pH` also returned: `[(coverageIncomplete,null),(staleEvidence,null),(staleEvidence,pH)]`. Selected `complete` plus only minted possibly-stale: `[(staleEvidence,null)]`. A current unselected `omitted` row alone adds no `coverageIncomplete`. `staleTarget=true` and its warning are invalid, not a positive vector. Warning ordering follows warnings-v1 (code enum, then null-before-text); `partial`/boundary behavior follows the unchanged graph contract.
