# Decision 0018: semantic symbol retrieval

- Status: owner-ratified direction on #211 (2026-10-21), amended by the 2026-10-25 parity and
  backend rulings. Ranking constants remain pending the owner-required tuning and contract
  review; this record does not authorize shipping arbitrary constants.
- Amends: #24’s catalog/wire projection and #23’s optional-vector-cache topology as stated in
  their amended sections. No #22 evidence record, identity, provenance or warning shape changes.
- Scope: one card/vector per eligible named declaration; no AST chunks, summary vectors,
  hosted provider, score field or debug-score flag.
- Compatibility: none for disposable indexes. Native search-fact output changes the native
  producer version; storage changes follow the current disposable-index contract. Allocate
  neither schema nor native-producer version until the feature-scip/main integration is synced.

## Authority and activation boundary

- [D1–D13 and accepted proposal defaults, 2026-10-21](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6324615865).
- [Chosen form-c parity acceptance](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6405701173).
- [Automatic backend and status ruling](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6405742145).

These amendments record that direction; they do not ratify the old plan/proposal wholesale.
Search is specified, not enabled by this records-only change. Representation details listed
below, the backend refusal mapping and evaluated fusion constants need reviewed closure
before their respective implementation is wired or activated. Synchronize main after
`feature-scip` merges before index-facing changes; this record allocates no schema or producer.

## Cards and measured inputs

The chosen card form is (c): measured name/kind, separately labelled measured named ancestors,
Rust impl self type and trait, root-relative path, signature from stored header/parameters/header
items, doc comment, bounded body facts, and a capped source head. Callee spellings come from
native calls; type names, string literals, error-code-shaped literal tokens and container
member names are measured from the native extraction tree/captured bytes. Nothing is parsed
from displayKey and the card does not use the path#qualified-name presentation format.

Capture previously missing doc comments, literals, body type names and impl self/trait facts
during the existing native parse. No second parse, file reread or read-time extraction is
introduced. Store these in search-only version-owned tables, not #22 Declaration/Header or
another evidence record. Retain the proposed bounds: at most 20 literals of at most 100 bytes,
a 600-byte doc comment, 30 type names and 40 callees per declaration. The exact card serializer,
source-head cap, candidate-kind set and versioned stopword list must be written and reviewed
before card/FTS wiring; this record invents none of their missing bytes or membership rules.
Use the chosen (c) reference/evaluation artifacts when closing that format.

cardHash = SHA-256(UTF8("trellis.search-card.v1\0") || exactCardBytes).
Cards include the measured root-relative path. Equal exact card bytes can reuse vectors
across checkouts. Card-format changes change modelId even when extraction is unchanged. They also regenerate affected current-head card bytes/hashes and ranking FTS rows through coherent publication; changing only the vector-cache identity would leave stale lexical rows. Derived state that is being rebuilt need not be validated first.

## Model identity and runtime

modelId = SHA-256(UTF8("trellis.search-model.v1\0") || canonicalJSON(descriptor)), reported as
64 lowercase hex. Use #22’s canonical-JSON byte rules. The descriptor has exactly
{name,revision,modelFiles,dimensions,quantization,maxTokens,queryPrompt,documentPrompt,pooling,
inputPolicy,cardFormat}. `modelFiles` is a nonempty array of exactly `{path,sha256}` objects,
ordered by relative filename UTF-8 bytes, without duplicate filenames; each SHA-256 is 64
lowercase hex. It replaces the proposal’s two scalar weight/tokenizer hashes, covering all
verified snapshot files without an ambiguous multi-file hash concatenation. One internal
supported-model descriptor supplies both loader pins and modelId generation, not two copied
pin lists. No public descriptor API is required.

The initial supported snapshot is `voyageai/voyage-4-nano` at upstream revision
`5fb34e870f49f19e79c94d5e118f5ee730133279`. Its nine filename/hash pairs are the reviewed
`MODEL_FILES` in `src/search_embed/mod.rs` at PR #337 commit
`a32ac03aa44d8ac6858f1a153efe273d50cafed3`; integration preserves those exact pins. All nine
files are verified; only verified owned config/tokenizer/weight bytes are parsed, and custom
Python is provenance only, never executed. This records the existing runtime pin, not a new
model choice. Exact quantization/pooling/inputPolicy identifiers and cardFormat serialization
must be fixed in the supported descriptor review before model/cache wiring. `inputPolicy`
binds the accepted preprocessing: the 16,384-byte UTF-8-boundary text cap, exact prompt and
whole-prompted-string Python-compatible strip semantics, NFC, no BOS/EOS insertion, 512-token
right truncation, and right padding with token 151643 excluded from attention/pooling. The
runtime’s implementation and parity documentation define these existing operations; versioned
identity must change when their meaning changes. Backend/build identities are not cache-key
inputs, and this adds no cross-call numerical equality requirement.

One supported model is active at a time, selected from Trellis’s closed reviewed pins.
Configured files are verified before loading against the reviewed immutable model descriptor.
The model loads lazily for a job and drops afterward; its pin is not a promise of daemon-lifetime
residency. The single embedding owner also owns query-model availability and avoids blocking
checkout worker locks while loading. Its precise query/job lifecycle must preserve the owner’s
lazy-load/drop constraints and lexical-only state while loading. The model
is not a #22 Producer: vectors are retrieval metadata, not evidence. Other modelId cache
entries are ignored safely after switching models. The first model is voyageai/voyage-4-nano,
ported to Rust/Candle; no Python runtime or executed upstream custom code is used. The current
pin and parity artifacts identify the exact reviewed upstream revision. Updating weights or
custom modelling code requires the owner’s concept/role evaluation and custom-code review.
A copied evaluation revision or a moving “latest” label is not a new reviewed pin.

The chosen (c) parity acceptance is every-card cosine >=0.999 against the saved #214 Python
vectors and no regression in recall@10 or MRR on that evaluation, for both CPU and Metal.
The owner accepted the measured (c) CPU/Metal results. Form (a) is unused and its diagnostic
slip is not a blocker; the old 0.07/0.08 metric tolerances are superseded, not retained gates.

Default backend selection is automatic: Metal on macOS with the metal feature, CPU elsewhere
including Linux/OrbStack. Automatic mode may fall back. Explicit Metal unavailability is a
typed error, never a silent fallback. searchIndex reports the actual backend; it does not
make backend selection into evidence. Release artifact/platform builds are #338’s scope.

## Lexical query and ranking

The search table is head-only, using `porter unicode61 remove_diacritics 2`, default categories
L* N* Co, with no tokenchars/separators overrides. `_` is a separator. Card text and query
share the same measured-text splitter. Take maximal runs of unicode61 token characters,
split at camelCase and letter-digit boundaries, keep the run and its parts, drop terms shorter
than two characters or in the fixed versioned stopword list, deduplicate in first-occurrence
order, and retain at most 32 terms for both card and query, as the accepted proposal specifies.
Insert the card’s transformed terms as text into FTS. Only query terms are emitted as quoted
FTS5 string literals, joined with ` OR ` and bound as a SQL parameter to MATCH. Card insertion
is not a MATCH expression. Raw caller text never enters the FTS query parser.
No terms gives an empty lexical arm before FTS runs, not an error or post-error fallback.
Errors after FTS runs follow the existing typed contention/store rules, never raw-query retry
or vector-only best-effort success. Decision 0013’s two trigram tables/rules are unchanged.

Candidates are unique by (document version, symbolId). The lexical arm ranks eligible head
candidates by BM25 ascending then symbolId bytes and cuts at depth D; l is its 1-based rank.
The vector arm ranks candidates with current decoded vectors by cosine descending then
symbolId bytes, also at depth D; v is its 1-based rank. Fusion is
wL/(k+l) + wV/(k+v); an absent arm contributes zero. Sort score descending, then smaller
available arm rank, then l (absent last), then symbolId bytes. matchedBy records the arms
that returned the candidate, not an inferred relevance explanation.

k, D, wL and wV are NOT fixed by this record. Tune them on #214’s saved evaluation set during
implementation and land exact values through contract review before enabling the tool.
The old 60/100/1/1 proposal is not a default or ratified implementation choice. An
exact-match-limited lexical arm is permitted only if evaluation supports it and its precise
matching rule is reviewed and recorded along with the constants. Changes to the fixed rule
or constants after that review are contract changes, not silent tuning.

Within a call, one index snapshot and one vector-cache read transaction give coherent
candidates, counts and matchedBy/order. evidenceBasis pins only the index head, not the
asynchronous evictable cache. Re-embedding the same (modelId,cardHash) need not reproduce
identical bytes. Cache filling, eviction or changed numerical output may change retrieval
between calls at one basis. No cross-call equality, backend equality or runtime-independent
ranking guarantee is added. Existing within-call ordering and typed numerical validity must
still hold; non-finite/malformed vectors cannot enter cosine ordering as valid rows.

## Retrieval is not evidence

A hit’s syntax Evidence proves only the measured declaration, not relevance. There is no
relevance provenance, confidence or numeric score. Zero hits proves no absence. Agents
inspect the exact declaration and call views, with optional returned-basis guards that can
conflict after publication. Search relevance never reattaches a durable anchor, creates a
binding or supplies evidence for another tool. Native completeness remains W2’s closure and
scope rule; vector completeness and selected backend are separate searchIndex metadata.

## Required observed checks

1. Both catalogs list ten tools in the specified order, schemas stay closed, describe reports
   toolVersion 2, existing tools/prompt remain unchanged, and the existing catalog headroom
   check still passes. Search has no score/filter/backend input.
2. Native card facts come from the existing capture/parse; displayKey changes cannot change
   card bytes/rank; source edits change the appropriate version-owned card/hash atomically.
3. Head publication atomically updates search FTS rows; retaining/releasing old revisions
   cannot change head BM25 statistics. Both existing trigram paths remain byte-identical.
4. Same-basis per-call index/cache coherence, optional-pin conflict, emitted-only closure,
   zero-hit incomplete native scope, byte-prefix clipping and syntaxOnly are observed.
   Missing vectors alone never set partial/coverageIncomplete.
5. A missing/evicted/decode-invalid vector is a miss; actual database/schema/I/O faults fail
   typed with no hits; contention is bounded and typed, never hidden as cache unavailability.
   Restart recomputes misses; background work does not delay publication or reorder FIFO.
6. Supported-model hash refusal, modelId separation, cap/LRU, cross-worktree/cache reuse and
   read-only gc size/count reporting are observed at ordinary local-tool scale.
7. Automatic backend choice and allowed fallback are observed; explicit Metal unavailability
   is a typed refusal. Status reports the actual backend and separate completeness. Existing
   accepted CPU/Metal (c) parity evidence is referenced, not replaced by weaker tolerances.
8. The #214 tuning report, chosen constants and any lexical-arm restriction receive review
   and become exact record text before the new tool is enabled. Measure the built FTS table
   against #199’s size results. No hosted code/model use or invented shipping gate is added.
