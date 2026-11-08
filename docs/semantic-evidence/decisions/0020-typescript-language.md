# Decision 0020: TypeScript as its own language

- **Status:** owner-ratified 2026-11-07 (#683). Owner decision 2026-11-04: TypeScript is its own language `typescript`, in 0.2.
- **Amends:**
  - [contract v1](../contract-v1.md): `Language` row; #61 batch admission (requested languages; artifact `languages`); reference-candidate table and exclusions; the narrow alias rule and the corpus alias role; the lookup table and escape sentence; the role table and alias sentence; the #470/#557 dispatch classification; the #473 `constructor` kind exception; the stable-identifier signature and overload rows; and the new TypeScript amendment table.
  - [Decision 0017](0017-conformance-corpus-storage-and-producers.md): Amendment 7.
  - The [MCP read-only pilot contract](../../mcp-readonly-pilot-contract.md): the #683 amendment (fixed language rows, `Destination.language`, incoming-call lookup decoding, JavaScript/TypeScript architecture output).
  - Decision 0002 and Decision 0011 are unchanged.
- **Compatibility:** none (pre-release). Native producer `native-v16`, index schema 41; derived indexes are recreated. A retained scip-typescript run written before this change is refused once, typed, and must be re-run.

## Decision

1. **Language.** `typescript` joins the closed `Language` set after `javascript`. `.ts/.tsx/.mts/.cts` are TypeScript. `.js/.mjs/.cjs`, including JSDoc-typed JavaScript, stay `javascript`. `.jsx` stays unindexed.
2. **Declaration-only documents** (`.d.ts`, `.d.mts`, `.d.cts`) measure declarations, headers and reference candidates, with no calls or control regions; an accepted body is skipped without a gap. Their role follows Decision 0011 item 5's path rule like any document.
3. **Grammars.** One language; the `tsx` grammar for `.tsx`, the `typescript` grammar otherwise.
4. **Native measurement** follows the contract-v1 TypeScript amendment.
5. **scip-typescript covers JavaScript and TypeScript.** Its pinned profile lists both. One producer run indexes both, and its unchanged original artifact may contain documents of both. Each descriptor artifact entry names its `languages` (nonempty, unique, in `Language` order, a subset of the producer's languages); a covered document's language must be one of them. Decision 0002's #420 rule applies per document unchanged: an empty `Document.language` binds to the detected language because the profile covers it; a non-empty value must equal it exactly, so another spelling rejects the artifact with a typed error. No artifact bytes are rewritten.
6. **Dispatch.** The #470/#557 JavaScript classification and the #473 `constructor` kind exception apply to TypeScript. The own-language precondition is unchanged.
7. **MCP.** Fixed `LanguageStatus` rows (describe, incoming, lookup alternatives) are five in #22 order. Find and search keep the #715 result-language rule. `toolVersion` is unchanged.
8. **Structural import resolution** (read-time, not native measurement) uses one JavaScript/TypeScript lookup and the root `tsconfig.json` (else `jsconfig.json`) `baseUrl`, `paths` and `rootDirs`. `extends` and nested configs are not followed. Failures are typed unknowns.
