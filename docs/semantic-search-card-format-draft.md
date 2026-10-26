# Semantic search card format — review draft

2026-10-26 · #211 · **Draft only. Not ratified; does not enable search or authorize wiring.**

This is the narrow serializer, source-head and stopword proposal requested for review.
Decision [0018](semantic-evidence/decisions/0018-semantic-symbol-retrieval.md) remains
binding. The evidence input is `issue-211/next-slice-card-lexical-closure-draft.md`
in the external evidence directory; that input is itself a recommendation, not authority.
Runtime #337 and Decision 0018 (#339) are merged on main. This draft follows that
ratified direction without reopening the accepted runtime or authorizing index wiring.

## 1. Boundary between requirements and choices

Already required: form c; measured name/kind, separately labelled named ancestors,
Rust impl self type/trait, root-relative path, measured signature, doc, bounded body facts
and source head. No displayKey parsing, path#qualified-name representation, AST chunks,
second parse, source reread or read-time extraction. Bounds already required are a
600-byte doc, 20 literals of at most 100 bytes, 30 types and 40 callees.

**Every additional detail below is a proposed review choice, not a ratified fact.** In
particular, the 1,200-byte head, exact serializer, eligibility, fact attachment, signature
rendering and STOP membership require review. The proposed format contains only the
measured form-c fields; it adds no language, role, identity, location or debug metadata.
No schema/native-producer version, tool/schema change, index change, FTS wiring, ranking
constant or activation is allocated here. `cardFormat` is not fixed by publishing this draft.

Decision0018's **32 retained terms for both the whole card and the query** is preserved.
There is no uncapped card stream, per-field budget reset or hidden original-text append.

## 2. Eligibility and ownership — choices still requiring closure

Proposed closed candidate kinds, following the historical c reference:

```text
function method type implementation namespace module
```

Also require a non-null, nonempty measured declaration name. Both production and test
roles qualify. One card per eligible declaration; fields, variables, parameters, aliases
and anonymous functions are not promoted to cards. Never invent a name from a path,
stem, displayKey or ordinal.

**Constructor gap:** native Java `constructor` is outside this proposed set; its facts
would fold into the nearest eligible enclosing type. A JavaScript constructor classified
as `method` remains eligible. Whether to add `constructor` is an explicit review choice,
not an inference from “named declaration.”

**Module/impl gap:** the inspected synthetic file module has no measured name and gets
no card; neither does an unnamed Rust impl. A genuinely named module is eligible under
the proposed set; Rust named `mod` currently maps to namespace. Do not manufacture file
cards or impl names to close coverage gaps. Confirm these mappings against integrated
native facts before implementation. These unresolved choices do not authorize enabling.

Use lexical ancestors, outermost first: include named ancestors of any kind and Rust
impl ancestors with measured self type/optional trait; skip other unnamed ancestors.
Copy impl spellings from the existing parse, not a regex over a source prefix. Body facts
belong to their nearest eligible containing declaration; calls start from their measured
native owner and follow ancestry to that declaration. Facts with no eligible owner are
omitted. Do not cross a nested eligible declaration boundary. Members are named immediate
children of a container, excluding parameters, not recursive descendants. The contiguous
source head is deliberately different: it may include nested declaration text.

Doc attachment, string-payload selection and syntax-marked type extraction must still
receive native-capture review. This draft uses the syntactic policies in the external
closure draft as proposals, not already-existing production facts. No approximate regex
scan, capitalization inference, evaluated/interpolated literal or semantic resolution is
silently substituted for measured facts.

## 3. Value preparation and bounds — proposed exact operations

All text is valid UTF-8 measured from the same captured document/native parse.

- `prefix(s,n)` is the longest prefix of s taking at most n UTF-8 bytes and ending on a
  Unicode scalar boundary. Append no ellipsis or replacement character.
- `space(s)` collapses each maximal run of ASCII space, tab, CR or LF to one ASCII space,
  then trims those characters at both ends. No other whitespace change, Unicode
  normalization, case folding or language unescaping is implied.
- Body-fact lists and member names retain source order `(startByte,endByte,nativeOrdinal)`
  unless stated otherwise. Remove empty values and exact-byte duplicates after the
  specified preparation, keeping the first. Apply count caps after this deduplication.
- Ancestors retain lexical order and repeated names; Header arrays retain stored order
  and repeated values. Do not deduplicate or remove entries from these structural arrays.
- Names, path and impl spellings retain measured bytes. The path is the stored
  root-relative spelling, never an absolute checkout path.

| Field | Proposed preparation and limit |
| --- | --- |
| Doc | Strip syntactic doc delimiters/markers according to the capture policy; join attached payloads with LF; `prefix(space(payload),600)` |
| Strings | Source payloads with only syntactic prefixes/delimiters removed; preserve escape/interpolation spelling without evaluating it; `prefix(payload,100)` per value, deduplicate, retain 20; omit the selected Python docstring |
| Types | Measured signature parameter types, result type, then syntax-marked owned body types in source order; `space`, deduplicate, retain 30 |
| Calls | Measured native callee spellings with the ownership above; `space`, deduplicate, retain 40 |
| Error codes | From retained/truncated strings only: maximal ASCII `[A-Za-z0-9_]+` runs whose whole value matches `[a-z][a-z0-9]*(?:_[a-z0-9]+)+`; deduplicate in literal/run order |
| Members | Measured names of immediate container children in source order; deduplicate; only type/implementation/namespace/module cards |
| Head | `prefix(capturedSource[declaration.range],1200)`, then collapse maximal ASCII space/tab runs to one ASCII space; preserve CR/LF and every other byte |

The head cap is **1,200 source bytes before whitespace collapse**, not characters, escaped
bytes or model tokens. No trimming, dedenting, newline normalization, nested-body removal
or fallback first-line source extraction is added. Doc/literal caps likewise apply before
JSON escaping. Do not import the donor's extra 15-error, 40-member or 80-character-callee
caps. Error codes are bounded by the retained literals; signature/members use existing
native capture/work bounds, not new per-field read-side caps.

### Signature rendering

Use measured Header fields, never displayKey or a first-line fallback. Apply `space` to
each component. Join modifiers, kind and measured name with single ASCII spaces, omitting
empty components. If typeParameters is nonempty append `<` + their `, ` join + `>`.
Append `(` + parameters joined by `, ` + `)`, except for a container kind from the member
row above with no parameters. Render each parameter as optional `...`, measured name (or
`_` when absent), and optional `: ` + measured type. Append optional ` -> ` + resultType,
then optional ` : ` + bases joined by `, `. Do not repair bracketed spellings or infer
missing types. No extra signature cap is proposed.

## 4. Exact card serialization — proposed bytes

Use UTF-8 without BOM. `J(value)` means #22 canonical JSON serialization of that value,
with its existing escaping/key-order rules, compact and without optional whitespace.
The `J(...)` notation below denotes emitted JSON bytes, not the literal letters `J(...)`.
It is card text, **not** a new evidence/wire schema.

Emit these five lines unconditionally in exactly this order:

```text
kind: J(kind)
name: J(name)
ancestors: J(ancestors)
path: J(rootRelativePath)
signature: J(signature)
```

Each ancestor is an object with exactly `kind`, `name`, `selfType`, `trait`. Non-impl
ancestors have null selfType/trait. A Rust impl ancestor has its measured selfType and
optional trait, with null name if unnamed. The ancestors value is an array, including
`[]` when empty; canonical JSON determines object-key byte order. No joined qualified
name is added.

Then emit only nonempty fields, in exactly this order. Doc is a string; all others are
arrays of strings:

```text
doc: J(doc)
calls: J(callees)
types: J(types)
strings: J(literalPayloads)
error codes: J(errorTokens)
members: J(memberNames)
```

Join all emitted metadata lines with one LF. Append the exact bytes `\nbody:\n`, then
the prepared head bytes. Always emit the body marker, even for an empty head. Add **no
final LF** beyond any LF already present in the head. JSON escaping keeps newlines and
labels inside metadata values from creating metadata lines; the head stays literal text.
No language line, extra separator, root/revision/hash/range/ordinal/syntax ID, role or
displayKey appears in the card.

Hash these exact bytes using the already-required rule:

```text
cardHash = SHA-256(UTF8("trellis.search-card.v1\0") || exactCardBytes)
```

Do not hash a JSONL evaluation wrapper or transformed lexical terms. A reviewed format
change changes modelId through cardFormat and requires coherent regeneration of affected
current-head cards/hashes and lexical rows. This statement authorizes no implementation
or version allocation now.

## 5. Fixed STOP proposal and the 32-term rule

Proposed version-1 STOP membership is the historical evaluator's complete list, verbatim:

```text
a an the and or of to in on for by with from is are be it its that this which when where what how code function find does do into before after while still their there them than then only one
```

These exact lowercase ASCII words are the entire list; no library list, language-keyword
list or implicit additions such as `not`/`near` apply. `a` remains listed although the
short-term rule also removes it. A membership change requires review, not silent tuning.

Use Decision0018's shared splitter: maximal unicode61 token-character runs, whole run
plus camelCase/letter-number parts, short/STOP removal, first-occurrence deduplication,
**at most 32 terms across the entire input**. The format review must fix any remaining
case/acronym boundary details with a small shared example set; this serializer draft does
not introduce a second general Unicode engine or claim those details are already closed.
The required tokenizer remains `porter unicode61 remove_diacritics 2`, default L*/N*/Co,
no tokenchars/separators overrides, `_` a separator.

For cards, transform the entire exact card text once in serialization order, including
labels. Join retained terms with one ASCII space, no leading/trailing space; empty means
empty text. No quotes, `OR`, raw card append, per-line restart or second split stream.
Labels can consume the same 32 slots; changing that is a separate review choice.

For queries, use the same splitter/filter/dedup/32 rule. Emit each retained term as an FTS5
quoted string literal (double embedded quotes), join with ` OR `, and bind the expression.
Raw caller text never enters MATCH. No terms means an empty lexical arm before FTS runs;
post-FTS faults remain typed failures, not raw-query or vector-only fallback. These are
format descriptions, not permission to wire FTS.

The lexical 32-term cap does **not** shorten exactCardBytes before hashing or embedding.
Model inputPolicy retains its separate byte/token preprocessing limits. No hidden uncapped
lexical path is proposed to compensate for quality changes.

## 6. Evaluation and review before fusion

The accepted CPU/Metal form-c parity covers the saved c card inputs used in that runtime
evaluation. It does not certify these new serializer bytes, scalar-safe caps, native fact
selection or both-sides-32 lexical rows. Saved-card metrics cannot be relabelled as results
for new cards.

After format/capture/eligibility closure, measure the newly produced exact cards and exact
lexical transform against the saved #214 query/relevance set, retaining a clearly labelled
historical baseline. Review changed candidate coverage and retrieval quality **before
using the new representation to choose fusion constants**. Keep the accepted runtime
parity evidence; this is a representation evaluation, not a weaker replacement for it.
No quality result, fusion constant or new shipping gate is invented here. Unclosed
candidate/capture/splitter choices and unevaluated new card bytes mean **no enablement**.

Assumptions: the supplied Decision0018 is the authority baseline; historical c is reference
evidence, not production byte identity; the native facts must be confirmed after integration.
This draft changes no code, indexes, tools, schemas, native versions or ratified constants.
